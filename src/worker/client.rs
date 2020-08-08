use std::path::Path;

use actix::prelude::*;
use qmetaobject::*;

use crate::gui::StorageReady;
use crate::sfos::SailfishApp;
use crate::store::Storage;

use libsignal_protocol::Context;
use libsignal_service::content::{AttachmentPointer, ContentBody, DataMessage, GroupType};
use libsignal_service::prelude::*;
use libsignal_service_actix::prelude::*;

const SERVICE_URL: &str = "https://textsecure-service.whispersystems.org/";
const CDN_URL: &str = "https://cdn.signal.org";
const CDN2_URL: &str = "https://cdn.signal.org";
const ROOT_CA: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", "rootCA.crt"));

#[derive(Message)]
#[rtype(result = "()")]
struct AttachmentDownloaded(i32);

#[derive(QObject, Default)]
#[allow(non_snake_case)]
pub struct ClientWorker {
    base: qt_base_class!(trait QObject),
    messageReceived: qt_signal!(sid: i64, mid: i32),
    messageReceipt: qt_signal!(sid: i64, mid: i32),
    notifyMessage: qt_signal!(sid: i64, source: QString, message: QString, is_group: bool),
    promptResetPeerIdentity: qt_signal!(),

    actor: Option<Addr<ClientActor>>,
}

/// ClientActor keeps track of the connection state.
pub struct ClientActor {
    inner: QObjectBox<ClientWorker>,

    /// Some(Service) when connected, otherwise None
    service: Option<AwcPushService>,
    storage: Option<Storage>,
    cipher: Option<ServiceCipher>,
    context: Context,
}

impl ClientActor {
    pub fn new(app: &mut SailfishApp) -> Result<Self, failure::Error> {
        let inner = QObjectBox::new(ClientWorker::default());
        app.set_object_property("ClientWorker".into(), inner.pinned());

        let crypto = libsignal_protocol::crypto::DefaultCrypto::default();

        Ok(Self {
            inner,
            service: None,
            storage: None,
            cipher: None,
            context: Context::new(crypto)?,
        })
    }

    /// Downloads the attachment in the background and registers it in the database.
    /// Saves the given attachment into a random-generated path. Saves the path in the database.
    ///
    /// This was a Message method in Go
    pub fn handle_attachment(
        &mut self,
        ctx: &mut <Self as Actor>::Context,
        mid: i32,
        dest: impl AsRef<Path> + 'static,
        ptr: AttachmentPointer,
    ) {
        use futures::future::FutureExt;

        let client_addr = ctx.address();

        let mut service = self.service.clone().unwrap();
        let mut storage = self.storage.clone().unwrap();

        // Sailfish and/or Rust needs "image/jpg" and some others need coaching
        // before taking a wild guess
        let ext = match ptr.content_type() {
            "text/plain" => "txt",
            "image/jpeg" => "jpg",
            "image/jpg" => "jpg",
            other => mime_guess::get_mime_extensions_str(other)
                .expect("Could not find mime")
                .first()
                .unwrap(),
        };
        Arbiter::spawn(
            async move {
                use futures::io::AsyncReadExt;
                use libsignal_service::attachment_cipher::*;

                let mut stream = service.get_attachment(&ptr).await?;
                log::info!("Downloading attachment");

                // We need the whole file for the crypto to check out 😢
                let mut ciphertext = if let Some(size) = ptr.size {
                    Vec::with_capacity(size as usize)
                } else {
                    Vec::new()
                };
                let len = stream.read_to_end(&mut ciphertext).await
                    .expect("streamed attachment");

                // Downloaded attachment length (1781792) is not equal to expected length of 1708516 bytes.
                // Not sure where the difference comes from at this point.
                if len != ptr.size.unwrap() as usize {
                    log::warn!("Downloaded attachment length ({}) is not equal to expected length of {} bytes.", len, ptr.size.unwrap());
                }
                let key_material = ptr.key.expect("attachment with key");
                assert_eq!(
                    key_material.len(),
                    64,
                    "key material for attachments is ought to be 64 bytes"
                );
                let mut key = [0u8; 64];
                key.copy_from_slice(&key_material);
                decrypt_in_place(key, &mut ciphertext).expect("attachment decryption");

                let attachment_path =
                    crate::store::save_attachment(&dest, ext, futures::io::Cursor::new(ciphertext))
                        .await;

                storage.register_attachment(
                    mid,
                    attachment_path.to_str().expect("attachment path utf-8"),
                );
                client_addr.send(AttachmentDownloaded(mid)).await?;
                Ok(())
            }
            .map(|r: Result<(), failure::Error>| {
                if let Err(e) = r {
                    log::error!("Error fetching attachment: {:?}", e);
                }
            }),
        )
    }

    /// Process incoming message from Signal
    ///
    /// This was `MessageHandler` in Go.
    ///
    /// TODO: consider putting this as an actor `Handle<>` implementation instead.
    pub fn handle_message(
        &mut self,
        ctx: &mut <Self as Actor>::Context,
        source: String,
        msg: DataMessage,
        is_sync_sent: bool,
        timestamp: u64,
    ) {
        let settings = crate::settings::Settings::default();

        let storage = self.storage.as_mut().expect("storage");

        let mut new_message = crate::store::NewMessage {
            source: source,
            text: msg.body().into(),
            flags: msg.flags() as i32,
            outgoing: is_sync_sent,
            sent: is_sync_sent,
            timestamp: if is_sync_sent && timestamp > 0 {
                timestamp
            } else {
                msg.timestamp()
            } as i64,
            has_attachment: msg.attachments.len() > 0,
            mime_type: None,  // Attachments are further handled asynchronously
            received: false,  // This is set true by a receipt handler
            session_id: None, // Canary value checked later
            attachment: None,
        };

        let group = if let Some(group) = msg.group.as_ref() {
            match group.r#type() {
                GroupType::Update => {
                    new_message.text = String::from("Member joined group");
                }
                GroupType::Quit => {
                    new_message.text = String::from("Member left group");
                }
                t => log::warn!("Unhandled group type {:?}", t),
            }

            Some(crate::store::NewGroup {
                id: group.id(),
                name: group.name().to_string(),
                members: group.members_e164.clone(),
            })
        } else {
            None
        };

        let is_unread = !new_message.sent.clone();
        let (message, session) = storage.process_message(new_message, group, is_unread);

        if settings.get_bool("save_attachments") && !settings.get_bool("incognito") {
            for attachment in msg.attachments {
                // Go used to always set has_attachment and mime_type, but also
                // in this method, as well as the generated path.
                // We have this function that returns a filesystem path, so we can
                // set it ourselves.
                let dir = settings.get_string("attachment_dir");
                let dest = Path::new(&dir);

                self.handle_attachment(ctx, message.id, dest.to_path_buf(), attachment);
            }
        }

        self.inner
            .pinned()
            .borrow_mut()
            .messageReceived(session.id, message.id);
        self.inner.pinned().borrow_mut().notifyMessage(
            session.id,
            session
                .group_name
                .as_deref()
                .unwrap_or(&session.source)
                .into(),
            message.message.into(),
            session.group_id.is_some(),
        );
    }

    fn process_receipt(&mut self, msg: Envelope) {
        log::info!("Received receipt: {}", msg.timestamp());

        // XXX: figure out edge cases in which these are *not* initialized.
        let storage = self.storage.as_mut().expect("storage initialized");

        // XXX: this should probably not be based on ts alone.
        let ts = msg.timestamp();
        let source = msg.source_e164();
        // XXX should this not be encrypted and authenticated?
        log::trace!("Marking message from {} at {} as received.", source, ts);
        if let Some((sess, msg)) = storage.mark_message_received(ts) {
            self.inner
                .pinned()
                .borrow_mut()
                .messageReceipt(sess.id, msg.id)
        }
    }

    fn process_message(&mut self, msg: Envelope, ctx: &mut <Self as Actor>::Context) {
        // XXX: figure out edge cases in which these are *not* initialized.
        let storage = self.storage.as_mut().expect("storage initialized");
        let cipher = self.cipher.as_mut().expect("cipher initialized");

        let Content { body, metadata } = match cipher.open_envelope(msg) {
            Ok(Some(content)) => content,
            Ok(None) => {
                log::info!("Empty envelope");
                return;
            }
            Err(e) => {
                log::error!("Error opening envelope: {:?}", e);
                return;
            }
        };

        log::trace!(
            "Opened envelope Content {{ body: {:?}, metadata: {:?} }}",
            body,
            metadata
        );

        match body {
            ContentBody::DataMessage(message) => self.handle_message(
                ctx,
                metadata.sender.e164.clone(),
                message,
                false,
                metadata.timestamp,
            ),
            ContentBody::SynchronizeMessage(message) => {
                if let Some(sent) = message.sent {
                    log::trace!("Sync sent message");
                    // These are messages sent through a paired device.

                    let message = sent.message.expect("sync sent with message");
                    self.handle_message(
                        ctx,
                        // Empty string mainly when groups,
                        // but maybe needs a check. TODO
                        sent.destination_e164.clone().unwrap_or("".into()),
                        message,
                        true,
                        0,
                    );
                } else if let Some(_request) = message.request {
                    log::trace!("Sync request message");
                } else if message.read.len() > 0 {
                    log::trace!("Sync read message");
                    for read in &message.read {
                        // XXX: this should probably not be based on ts alone.
                        let ts = read.timestamp();
                        let source = read.sender_e164();
                        log::trace!("Marking message from {} at {} as received.", source, ts);
                        if let Some((sess, msg)) = storage.mark_message_received(ts) {
                            self.inner
                                .pinned()
                                .borrow_mut()
                                .messageReceipt(sess.id, msg.id)
                        } else {
                            log::warn!("Could not mark as received!");
                        }
                    }
                } else {
                    log::warn!("Sync message without known sync type");
                }
            }
            ContentBody::TypingMessage(_typing) => {
                log::info!("{} is typing.", metadata.sender.e164);
            }
            ContentBody::ReceiptMessage(receipt) => {
                log::info!("{} received a message.", metadata.sender.e164);
                for ts in &receipt.timestamp {
                    if let Some((sess, msg)) = storage.mark_message_received(*ts) {
                        self.inner
                            .pinned()
                            .borrow_mut()
                            .messageReceipt(sess.id, msg.id)
                    } else {
                        log::warn!("Could not mark {} as received!", ts);
                    }
                }
            }
            ContentBody::CallMessage(_call) => {
                log::info!("{} is calling.", metadata.sender.e164);
            }
        }
    }
}

impl Actor for ClientActor {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.inner.pinned().borrow_mut().actor = Some(ctx.address());
    }
}

impl Handler<AttachmentDownloaded> for ClientActor {
    type Result = ();

    fn handle(
        &mut self,
        AttachmentDownloaded(mid): AttachmentDownloaded,
        _ctx: &mut Self::Context,
    ) {
        log::info!("Attachment downloaded for message {:?}", mid);
        // XXX: refresh Qt views
    }
}

impl Handler<StorageReady> for ClientActor {
    type Result = ResponseActFuture<Self, ()>;

    fn handle(
        &mut self,
        StorageReady(storage, config): StorageReady,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.storage = Some(storage.clone());

        let useragent = format!("Whisperfish-{}", env!("CARGO_PKG_VERSION"));
        let service_cfg = ServiceConfiguration {
            service_urls: vec![SERVICE_URL.to_string()],
            cdn_urls: vec![CDN_URL.to_string(), CDN2_URL.to_string()],
            contact_discovery_url: vec![],
        };

        let context = self.context.clone();

        Box::pin(
            async move {
                // Web socket
                let phonenumber = phonenumber::parse(None, config.tel).unwrap();
                let e164 = phonenumber
                    .format()
                    .mode(phonenumber::Mode::E164)
                    .to_string();
                log::info!("E164: {}", e164);
                let password = Some(storage.signal_password().await.unwrap());
                let signaling_key = storage.signaling_key().await.unwrap();
                let credentials = Credentials {
                    uuid: None,
                    e164: e164.clone(),
                    password,
                    signaling_key,
                };

                let service =
                    AwcPushService::new(service_cfg, credentials.clone(), &useragent, &ROOT_CA);
                let mut receiver = MessageReceiver::new(service.clone());

                let pipe = receiver.create_message_pipe(credentials).await.unwrap();
                let stream = pipe.stream();
                // end web socket

                // Signal service context
                let store_context = libsignal_protocol::store_context(
                    &context,
                    // Storage is a pointer-to-shared-storage
                    storage.clone(),
                    storage.clone(),
                    storage.clone(),
                    storage.clone(),
                )
                .expect("initialized storage");
                let local_addr = ServiceAddress {
                    uuid: None,
                    e164,
                    relay: None,
                };
                let cipher = ServiceCipher::from_context(context, local_addr, store_context);
                // end signal service context

                (cipher, service, stream)
            }
            .into_actor(self)
            .map(move |(cipher, service, pipe), act, ctx| {
                act.service = Some(service);
                act.cipher = Some(cipher);
                ctx.add_stream(pipe);
            }),
        )
    }
}

impl StreamHandler<Result<Envelope, ServiceError>> for ClientActor {
    fn handle(&mut self, msg: Result<Envelope, ServiceError>, ctx: &mut Self::Context) {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                // XXX: we might want to dispatch on this error.
                log::error!("MessagePipe pushed an error: {:?}", e);
                return;
            }
        };

        if msg.is_receipt() {
            self.process_receipt(msg);
        } else if msg.is_prekey_signal_message()
            || msg.is_signal_message()
            || msg.is_unidentified_sender()
        {
            self.process_message(msg, ctx);
        } else {
            log::warn!("Unknown envelope type {:?}", msg.r#type());
        }
    }
}
