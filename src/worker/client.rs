use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use actix::prelude::*;
use chrono::prelude::*;
use futures::prelude::*;
use phonenumber::PhoneNumber;
use qmetaobject::prelude::*;

use crate::actor::{LoadAllSessions, SessionActor};
use crate::gui::StorageReady;
use crate::model::DeviceModel;
use crate::qmlapp::QmlApp;
use crate::store::{orm, Storage};

use libsignal_service::configuration::SignalServers;
use libsignal_service::content::sync_message::Request as SyncRequest;
use libsignal_service::content::DataMessageFlags;
use libsignal_service::content::{
    sync_message, AttachmentPointer, ContentBody, DataMessage, GroupContext, GroupContextV2,
    GroupType, Metadata,
};
use libsignal_service::prelude::protocol::*;
use libsignal_service::prelude::*;
use libsignal_service::push_service::DEFAULT_DEVICE_ID;
use libsignal_service::sender::AttachmentSpec;
use libsignal_service::AccountManager;
use libsignal_service_actix::prelude::*;

pub use libsignal_service::provisioning::VerificationCodeResponse;
use libsignal_service::provisioning::{ConfirmCodeResponse, ProvisioningManager};
pub use libsignal_service::push_service::DeviceInfo;

mod migrations;
use migrations::*;

mod linked_devices;
pub use linked_devices::*;

mod profile;
pub use profile::*;

mod groupv2;
pub use groupv2::*;

use crate::millis_to_naive_chrono;

#[derive(Message)]
#[rtype(result = "()")]
/// Enqueue a message on socket by MID
pub struct SendMessage(pub i32);

#[derive(Message)]
#[rtype(result = "()")]
struct AttachmentDownloaded(i32);

#[derive(Message)]
#[rtype(result = "usize")]
pub struct CompactDb(usize);

#[derive(QObject, Default)]
#[allow(non_snake_case)]
pub struct ClientWorker {
    base: qt_base_class!(trait QObject),
    messageReceived: qt_signal!(sid: i32, mid: i32),
    messageReceipt: qt_signal!(sid: i32, mid: i32),
    notifyMessage: qt_signal!(
        sid: i32,
        sessionName: QString,
        senderIdentifier: QString,
        message: QString,
        isGroup: bool
    ),
    promptResetPeerIdentity: qt_signal!(),
    messageSent: qt_signal!(sid: i32, mid: i32, message: QString),

    connected: qt_property!(bool; NOTIFY connectedChanged),
    connectedChanged: qt_signal!(),

    actor: Option<Addr<ClientActor>>,
    session_actor: Option<Addr<SessionActor>>,
    device_model: Option<QObjectBox<DeviceModel>>,

    // Linked device management
    link_device: qt_method!(fn(&self, tsurl: String)),
    unlink_device: qt_method!(fn(&self, id: i64)),
    reload_linked_devices: qt_method!(fn(&self)),
    compress_db: qt_method!(fn(&self)),

    refresh_group_v2: qt_method!(fn(&self, session_id: usize)),
}

/// ClientActor keeps track of the connection state.
pub struct ClientActor {
    inner: QObjectBox<ClientWorker>,

    credentials: Option<ServiceCredentials>,
    local_addr: Option<ServiceAddress>,
    storage: Option<Storage>,
    // XXX The cipher should be behind a Mutex.
    // By considering the session that needs to be accessed,
    // we could lock only a single session to enforce serialized access.
    // That's a lot of code though, and it should probably happen *inside* the ServiceCipher
    // instead.
    // Having ServiceCipher implement `Clone` is imo. a problem, now that everything is `async`.
    // Putting in behind a Mutex is a lot of work now though,
    // especially considering this should be *internal* to ServiceCipher.
    cipher: Option<
        ServiceCipher<
            crate::store::Storage,
            crate::store::Storage,
            crate::store::Storage,
            crate::store::Storage,
            rand::rngs::ThreadRng,
        >,
    >,
    config: std::sync::Arc<crate::config::SignalConfig>,

    start_time: DateTime<Local>,
}

impl ClientActor {
    pub fn new(
        app: &mut QmlApp,
        session_actor: Addr<SessionActor>,
        config: std::sync::Arc<crate::config::SignalConfig>,
    ) -> Result<Self, anyhow::Error> {
        let inner = QObjectBox::new(ClientWorker::default());
        let device_model = QObjectBox::new(DeviceModel::default());
        app.set_object_property("ClientWorker".into(), inner.pinned());
        app.set_object_property("DeviceModel".into(), device_model.pinned());

        inner.pinned().borrow_mut().session_actor = Some(session_actor);
        inner.pinned().borrow_mut().device_model = Some(device_model);

        Ok(Self {
            inner,
            credentials: None,
            local_addr: None,
            storage: None,
            cipher: None,
            config,

            start_time: Local::now(),
        })
    }

    fn uuid(&self) -> Option<Uuid> {
        self.credentials.as_ref().and_then(|c| c.uuid)
    }

    fn user_agent(&self) -> String {
        crate::user_agent()
    }

    fn unauthenticated_service(&self) -> AwcPushService {
        let service_cfg = self.service_cfg();
        AwcPushService::new(service_cfg, None, self.user_agent())
    }

    fn authenticated_service_with_credentials(
        &self,
        credentials: ServiceCredentials,
    ) -> AwcPushService {
        let service_cfg = self.service_cfg();

        AwcPushService::new(service_cfg, Some(credentials), self.user_agent())
    }

    /// Panics if no authentication credentials are set.
    fn authenticated_service(&self) -> AwcPushService {
        self.authenticated_service_with_credentials(self.credentials.clone().unwrap())
    }

    fn message_sender(
        &self,
    ) -> MessageSender<
        AwcPushService,
        crate::store::Storage,
        crate::store::Storage,
        crate::store::Storage,
        crate::store::Storage,
        rand::rngs::ThreadRng,
    > {
        let storage = self.storage.clone().unwrap();
        let service = self.authenticated_service();
        MessageSender::new(
            service,
            self.cipher.clone().unwrap(),
            rand::thread_rng(),
            storage.clone(),
            storage,
            self.local_addr.clone().unwrap(),
            DEFAULT_DEVICE_ID,
        )
    }

    fn service_cfg(&self) -> ServiceConfiguration {
        // XXX: read the configuration files!
        SignalServers::Production.into()
    }

    /// Process incoming message from Signal
    ///
    /// This was `MessageHandler` in Go.
    ///
    /// TODO: consider putting this as an actor `Handle<>` implementation instead.
    pub fn handle_message(
        &mut self,
        ctx: &mut <Self as Actor>::Context,
        source_e164: Option<String>,
        source_uuid: Option<String>,
        msg: DataMessage,
        is_sync_sent: bool,
        timestamp: u64,
    ) {
        let settings = crate::config::Settings::default();

        let storage = self.storage.as_mut().expect("storage");
        let sender_recipient = if source_e164.is_some() || source_uuid.is_some() {
            Some(storage.merge_and_fetch_recipient(
                source_e164.as_deref(),
                source_uuid.as_deref(),
                crate::store::TrustLevel::Certain,
            ))
        } else {
            None
        };

        if msg.flags() & DataMessageFlags::EndSession as u32 != 0 {
            let storage = storage.clone();
            let source_e164 = source_e164.clone();
            let source_uuid = source_uuid.clone();
            actix::spawn(async move {
                if let Some(e164) = source_e164.as_ref() {
                    if let Err(e) = storage.delete_all_sessions(e164).await {
                        log::error!(
                            "End session (e164) requested, but could not end session: {:?}",
                            e
                        );
                    }
                }
                if let Some(uuid) = source_uuid.as_ref() {
                    if let Err(e) = storage.delete_all_sessions(uuid).await {
                        log::error!(
                            "End session (uuid) requested, but could not end session: {:?}",
                            e
                        );
                    }
                }
            });
        }

        if msg.flags() & DataMessageFlags::ExpirationTimerUpdate as u32 != 0 {
            // XXX Update expiration timer and notify UI
        }

        if msg.flags() & DataMessageFlags::ProfileKeyUpdate as u32 != 0 {
            // XXX Update profile key (which happens just below); don't insert this message.
        }

        if source_e164.is_some() || source_uuid.is_some() {
            if let Some(key) = msg.profile_key.as_deref() {
                storage.update_profile_key(
                    source_e164.as_deref(),
                    source_uuid.as_deref(),
                    key,
                    crate::store::TrustLevel::Certain,
                );
            }
        }

        if msg.flags() & DataMessageFlags::ProfileKeyUpdate as u32 != 0 {
            log::info!("Message was ProfileKeyUpdate; not inserting.");
        }

        let alt_body = if let Some(reaction) = &msg.reaction {
            Some(format!(
                "R@{}:{}",
                reaction.target_sent_timestamp(),
                reaction.emoji()
            ))
        } else if msg.flags() & DataMessageFlags::ExpirationTimerUpdate as u32 != 0 {
            Some(format!("Expiration timer has been changed ({:?} seconds), but unimplemented in Whisperfish.", msg.expire_timer))
        } else if let Some(GroupContextV2 {
            group_change: Some(ref _group_change),
            ..
        }) = msg.group_v2
        {
            Some(format!(
                "Group changed by {}",
                source_e164
                    .as_deref()
                    .or_else(|| source_uuid.as_deref())
                    .unwrap_or("nobody")
            ))
        } else if !msg.attachments.is_empty() {
            log::trace!("Received an attachment without body, replacing with empty text.");
            Some("".into())
        } else if msg.sticker.is_some() {
            log::warn!("Received a sticker, but inserting empty message.");
            Some("".into())
        } else if msg.payment.is_some()
            || msg.delete.is_some()
            || msg.group_call_update.is_some()
            || !msg.contact.is_empty()
        {
            Some("Unimplemented message type".into())
        } else {
            None
        };

        let body = msg.body.clone().or(alt_body);
        let text = if let Some(body) = body {
            body
        } else {
            log::debug!("Message without (alt) body, not inserting");
            return;
        };

        let mut new_message = crate::store::NewMessage {
            source_e164,
            source_uuid,
            text,
            flags: msg.flags() as i32,
            outgoing: is_sync_sent,
            sent: is_sync_sent,
            timestamp: millis_to_naive_chrono(if is_sync_sent && timestamp > 0 {
                timestamp
            } else {
                msg.timestamp()
            } as u64),
            has_attachment: !msg.attachments.is_empty(),
            mime_type: None,  // Attachments are further handled asynchronously
            received: false,  // This is set true by a receipt handler
            session_id: None, // Canary value checked later
            attachment: None,
            is_read: is_sync_sent,
        };

        let group = if let Some(group) = msg.group.as_ref() {
            match group.r#type() {
                GroupType::Update => {
                    new_message.text = String::from("Group was updated");
                }
                GroupType::Quit => {
                    new_message.text = String::from("Member left group");
                }
                t => log::warn!("Unhandled group type {:?}", t),
            }

            Some(
                storage.fetch_or_insert_session_by_group_v1(&crate::store::GroupV1 {
                    id: group.id().to_vec(),
                    name: group.name().to_string(),
                    members: group.members_e164.clone(),
                }),
            )
        } else if let Some(group) = msg.group_v2.as_ref() {
            let mut key_stack = [0u8; zkgroup::GROUP_MASTER_KEY_LEN];
            key_stack.clone_from_slice(group.master_key.as_ref().expect("group message with key"));
            let key = GroupMasterKey::new(key_stack);
            let secret = GroupSecretParams::derive_from_master_key(key);

            let store_v2 = crate::store::GroupV2 {
                secret,
                revision: group.revision(),
            };

            // XXX handle group.group_change like a real client
            if let Some(_change) = group.group_change.as_ref() {
                log::warn!("We're not handling raw group changes yet. Let's trigger a group refresh for now.");
                ctx.notify(RequestGroupV2Info(store_v2.clone()));
            } else if !storage.group_v2_exists(&store_v2) {
                log::info!(
                    "We don't know this group. We'll request it's structure from the server."
                );
                ctx.notify(RequestGroupV2Info(store_v2.clone()));
            }

            Some(storage.fetch_or_insert_session_by_group_v2(&store_v2))
        } else {
            None
        };

        let (message, session) = storage.process_message(new_message, group);

        if settings.get_bool("attachment_log") && !msg.attachments.is_empty() {
            log::trace!("Logging message to the attachment log");
            // XXX Sync code, but it's not the only sync code in here...
            let mut log = self.attachment_log();

            writeln!(
                log,
                "[{}] {:?} for message ID {}",
                Utc::now(),
                msg,
                message.id
            )
            .expect("write to the attachment log");
        }

        if settings.get_bool("save_attachments") && !settings.get_bool("incognito") {
            for attachment in msg.attachments {
                // Go used to always set has_attachment and mime_type, but also
                // in this method, as well as the generated path.
                // We have this function that returns a filesystem path, so we can
                // set it ourselves.
                let dir = settings.get_string("attachment_dir");
                let dest = Path::new(&dir);

                ctx.notify(FetchAttachment {
                    mid: message.id,
                    dest: dest.to_path_buf(),
                    ptr: attachment,
                });
            }
        }

        self.inner
            .pinned()
            .borrow_mut()
            .messageReceived(session.id, message.id);

        // XXX If from ourselves, skip
        if settings.get_bool("enable_notify") && !is_sync_sent {
            let session_name: &str = match &session.r#type {
                orm::SessionType::GroupV1(group) => &group.name,
                orm::SessionType::GroupV2(group) => &group.name,
                orm::SessionType::DirectMessage(recipient) => recipient.e164_or_uuid(),
            };

            self.inner.pinned().borrow_mut().notifyMessage(
                session.id,
                session_name.into(),
                sender_recipient
                    .map(|x| x.e164_or_uuid().into())
                    .unwrap_or_else(|| "".into()),
                message.text.as_deref().unwrap_or("").into(),
                session.is_group(),
            );
        }
    }

    fn handle_sync_request(&mut self, meta: Metadata, req: SyncRequest) {
        use sync_message::request::Type;
        log::trace!("Processing sync request {:?}", req.r#type());

        let local_addr = self.local_addr.clone().unwrap();
        let storage = self.storage.clone().unwrap();
        let mut sender = self.message_sender();

        actix::spawn(async move {
            match req.r#type() {
                Type::Unknown => {
                    log::warn!("Unknown sync request from {:?}:{}. Please upgrade Whisperfish or file an issue.", meta.sender, meta.sender_device);
                    return Ok(());
                }
                Type::Contacts => {
                    use libsignal_service::sender::ContactDetails;
                    // In fact, we should query for registered contacts instead of sessions here.
                    // https://gitlab.com/rubdos/whisperfish/-/issues/133
                    let recipients: Vec<orm::Recipient> = {
                        use crate::schema::recipients::dsl::*;
                        use diesel::prelude::*;
                        let db = storage.db
                            .lock();
                        recipients.load(&*db)?
                    };

                    let contacts = recipients.into_iter().map(|recipient| {
                            ContactDetails {
                                // XXX: expire timer from dm session
                                number: recipient.e164.clone(),
                                uuid: recipient.uuid.clone(),
                                name: recipient.profile_joined_name.clone(),
                                profile_key: recipient.profile_key,
                                // XXX other profile stuff
                                ..Default::default()
                            }
                    });

                    sender.send_contact_details(&local_addr, None, contacts, false, true).await?;
                },
                Type::Groups => {
                    use libsignal_service::sender::GroupDetails;
                    let sessions = storage.fetch_group_sessions();

                    let groups = sessions.into_iter().map(|session| {
                        let group = session.unwrap_group_v1();
                        let members = storage.fetch_group_members_by_group_v1_id(&group.id);
                        GroupDetails {
                            name: Some(group.name.clone()),
                            members_e164: members.iter().filter_map(|(_member, recipient)| recipient.e164.clone()).collect(),
                            // XXX: update proto file and add more.
                            // members: members.iter().filter_map(|(_member, recipient)| Member {e164: recipient.e164}).collect(),
                            // avatar, active?, color, ..., many cool things to add here!
                            // Tagging issue #204
                            ..Default::default()
                        }
                    });

                    sender.send_groups_details(&local_addr, None, groups, false).await?;
                }
                Type::Blocked => {
                    anyhow::bail!("Unimplemented {:?}", req.r#type());
                }
                Type::Configuration => {
                    anyhow::bail!("Unimplemented {:?}", req.r#type());
                }
                Type::Keys => {
                    anyhow::bail!("Unimplemented {:?}", req.r#type());
                }
            };

            Ok::<_, anyhow::Error>(())
        }.map(|v| if let Err(e) = v {log::error!("{:?}", e)}));
    }

    fn process_envelope(
        &mut self,
        Content { body, metadata }: Content,
        ctx: &mut <Self as Actor>::Context,
    ) {
        let storage = self.storage.as_mut().expect("storage initialized");

        match body {
            ContentBody::DataMessage(message) => {
                let e164 = metadata.sender.e164();
                let uuid = metadata.sender.uuid.map(|uuid| uuid.to_string());
                self.handle_message(ctx, e164, uuid, message, false, metadata.timestamp)
            }
            ContentBody::SynchronizeMessage(message) => {
                if let Some(sent) = message.sent {
                    log::trace!("Sync sent message");
                    // These are messages sent through a paired device.

                    let message = sent.message.expect("sync sent with message");
                    self.handle_message(
                        ctx,
                        // Empty string mainly when groups,
                        // but maybe needs a check. TODO
                        sent.destination_e164,
                        sent.destination_uuid,
                        message,
                        true,
                        0,
                    );
                } else if let Some(request) = message.request {
                    log::trace!("Sync request message");
                    self.handle_sync_request(metadata, request);
                } else if !message.read.is_empty() {
                    log::trace!("Sync read message");
                    for read in &message.read {
                        // XXX: this should probably not be based on ts alone.
                        let ts = read.timestamp();
                        let source = read.sender_e164();
                        // Signal uses timestamps in milliseconds, chrono has nanoseconds
                        let ts = millis_to_naive_chrono(ts);
                        log::trace!(
                            "Marking message from {} at {} ({}) as read.",
                            source,
                            ts,
                            read.timestamp()
                        );
                        if let Some((sess, msg)) = storage.mark_message_read(ts) {
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
                log::info!("{} is typing.", metadata.sender);
            }
            ContentBody::ReceiptMessage(receipt) => {
                log::info!("{} received a message.", metadata.sender);
                // XXX dispatch on receipt.type
                for &ts in &receipt.timestamp {
                    // Signal uses timestamps in milliseconds, chrono has nanoseconds
                    if let Some((sess, msg)) = storage.mark_message_received(
                        metadata.sender.e164().as_deref(),
                        metadata
                            .sender
                            .uuid
                            .as_ref()
                            .map(uuid::Uuid::to_string)
                            .as_deref(),
                        millis_to_naive_chrono(ts),
                        None,
                    ) {
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
                log::info!("{} is calling.", metadata.sender);
            }
        }
    }

    fn attachment_log(&self) -> std::fs::File {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.storage.as_ref().unwrap().path().join(format!(
                "attachments-{}.log",
                self.start_time.format("%Y-%m-%d_%H-%M")
            )))
            .expect("open attachment log")
    }
}

impl Actor for ClientActor {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.inner.pinned().borrow_mut().actor = Some(ctx.address());
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct FetchAttachment {
    mid: i32,
    dest: PathBuf,
    ptr: AttachmentPointer,
}

impl Handler<FetchAttachment> for ClientActor {
    type Result = ResponseActFuture<Self, ()>;

    /// Downloads the attachment in the background and registers it in the database.
    /// Saves the given attachment into a random-generated path. Saves the path in the database.
    ///
    /// This was a Message method in Go
    fn handle(
        &mut self,
        fetch: FetchAttachment,
        ctx: &mut <Self as Actor>::Context,
    ) -> Self::Result {
        let FetchAttachment { mid, dest, ptr } = fetch;

        let client_addr = ctx.address();

        let mut service = self.unauthenticated_service();
        let mut storage = self.storage.clone().unwrap();

        // Sailfish and/or Rust needs "image/jpg" and some others need coaching
        // before taking a wild guess
        let ext = match ptr.content_type() {
            "text/plain" => "txt",
            "image/jpeg" => "jpg",
            "image/jpg" => "jpg",
            "text/x-signal-plain" => "txt",
            "application/x-signal-view-once" => "bin",
            other => mime_guess::get_mime_extensions_str(other)
                .expect("Could not find mime")
                .first()
                .unwrap(),
        };

        let ptr2 = ptr.clone();
        Box::pin(
            async move {
                use futures::io::AsyncReadExt;
                use libsignal_service::attachment_cipher::*;

                let mut stream = loop {
                    let r = service.get_attachment(&ptr).await;
                    match r {
                        Ok(stream) => break stream,
                        Err(ServiceError::Timeout{ .. }) => {
                            log::warn!("get_attachment timed out, retrying")
                        },
                        Err(e) => return Err(e.into()),
                    }
                };
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
                let key_material = ptr.key();
                assert_eq!(
                    key_material.len(),
                    64,
                    "key material for attachments is ought to be 64 bytes"
                );
                let mut key = [0u8; 64];
                key.copy_from_slice(key_material);

                decrypt_in_place(key, &mut ciphertext).expect("attachment decryption");
                if let Some(size) = ptr.size {
                    log::debug!("Truncating attachment to {}B", size);
                    ciphertext.truncate(size as usize);
                }

                let attachment_path =
                    crate::store::save_attachment(&dest, ext, futures::io::Cursor::new(ciphertext))
                        .await;

                storage.register_attachment(
                    mid,
                    attachment_path.to_str().expect("attachment path utf-8"),
                    ptr.content_type(),
                );
                client_addr.send(AttachmentDownloaded(mid)).await?;
                Ok(())
            }
            .into_actor(self)
            .map(move |r: Result<(), anyhow::Error>, act, _ctx| {
                // Synchronise on the actor, to log the error to attachment.log
                if let Err(e) = r {
                    let e = format!("Error fetching attachment for message with ID `{}` {:?}: {:?}", mid, ptr2, e);
                    log::error!("{}", e);
                    let mut log = act.attachment_log();
                    if let Err(e) = writeln!(log, "{}", e) {
                        log::error!("Could not write error to error log: {}", e);
                    }
                }
            })
        )
    }
}

impl Handler<SendMessage> for ClientActor {
    type Result = ResponseActFuture<Self, ()>;

    // Equiv of worker/send.go
    fn handle(&mut self, SendMessage(mid): SendMessage, _ctx: &mut Self::Context) -> Self::Result {
        log::info!("ClientActor::SendMessage({:?})", mid);
        let mut sender = self.message_sender();
        let storage = self.storage.as_mut().unwrap();
        let msg = storage.fetch_message_by_id(mid).unwrap();

        let session = storage.fetch_session_by_id(msg.session_id).unwrap();

        if msg.sent_timestamp.is_some() {
            log::warn!("Message already sent, refusing to retransmit.");
            return Box::pin(async {}.into_actor(self).map(|_, _, _| ()));
        }

        let self_recipient = storage.fetch_self_recipient(&self.config);
        log::trace!("Sending for session: {:?}", session);
        log::trace!("Sending message: {:?}", msg);

        let local_addr = self.local_addr.clone().unwrap();
        let storage = storage.clone();
        Box::pin(
            async move {
                let group = if let orm::SessionType::GroupV1(group) = &session.r#type {
                    Some(GroupContext {
                        id: Some(hex::decode(&group.id).expect("hex encoded group id")),
                        r#type: Some(GroupType::Deliver.into()),
                        ..Default::default()
                    })
                } else {
                    None
                };
                let group_v2 = if let orm::SessionType::GroupV2(group) = &session.r#type {
                    let master_key = hex::decode(&group.master_key).expect("hex group id in db");
                    Some(GroupContextV2 {
                        master_key: Some(master_key),
                        revision: Some(group.revision as u32),
                        group_change: None,
                    })
                } else {
                    None
                };

                // XXX online status goes in that bool
                let online = false;
                let timestamp = msg.server_timestamp.timestamp_millis() as u64;
                let mut content = DataMessage {
                    body: msg.text.clone(),
                    flags: None,
                    timestamp: Some(timestamp),
                    // XXX: depends on the features in the message!
                    required_protocol_version: Some(0),
                    group,
                    group_v2,

                    profile_key: self_recipient.and_then(|r| r.profile_key),
                    ..Default::default()
                };

                if msg.flags == 1 {
                    log::warn!("End session unimplemented");
                }

                let attachments = storage.fetch_attachments_for_message(msg.id);

                for attachment in &attachments {
                    use actix_threadpool::BlockingError;
                    let attachment_path = attachment
                        .attachment_path
                        .clone()
                        .expect("attachment path when uploading");
                    let contents = match actix_threadpool::run(move || {
                        std::fs::read(&attachment_path)
                    })
                    .await
                    {
                        Err(BlockingError::Canceled) => {
                            anyhow::bail!("Threadpool Canceled");
                        }
                        Err(BlockingError::Error(e)) => {
                            anyhow::bail!("Could not read attachment: {}", e);
                        }
                        Ok(contents) => contents,
                    };
                    let attachment_path = attachment.attachment_path.as_ref().unwrap();
                    let spec = AttachmentSpec {
                        content_type: mime_guess::from_path(&attachment_path)
                            .first()
                            .unwrap()
                            .essence_str()
                            .into(),
                        length: contents.len(),
                        file_name: Path::new(&attachment_path)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned()),
                        preview: None,
                        voice_note: Some(attachment.is_voice_note),
                        borderless: Some(attachment.is_borderless),
                        width: attachment.width.map(|x| x as u32),
                        height: attachment.height.map(|x| x as u32),
                        caption: None,
                        blur_hash: None,
                    };
                    let ptr = match sender.upload_attachment(spec, contents).await {
                        Ok(v) => v,
                        Err(e) => {
                            anyhow::bail!("Failed to upload attachment: {}", e);
                        }
                    };
                    content.attachments.push(ptr);
                }

                log::trace!("Transmitting {:?}", content);

                match &session.r#type {
                    orm::SessionType::GroupV1(group) => {
                        let members = storage.fetch_group_members_by_group_v1_id(&group.id);
                        let members = members
                            .iter()
                            .filter_map(|(_member, recipient)| {
                                let member = recipient.to_service_address();

                                if local_addr.matches(&member) {
                                    None
                                } else {
                                    Some(member)
                                }
                            })
                            .collect::<Vec<_>>();
                        // Clone + async closure means we can use an immutable borrow.
                        let results = sender
                            .send_message_to_group(&members, None, content, timestamp, online)
                            .await;
                        for result in results {
                            if let Err(e) = result {
                                anyhow::bail!("Error sending message: {}", e);
                            }
                        }
                    }
                    orm::SessionType::GroupV2(group) => {
                        let members = storage.fetch_group_members_by_group_v2_id(&group.id);
                        let members = members
                            .iter()
                            .filter_map(|(_member, recipient)| {
                                let member = recipient.to_service_address();

                                if local_addr.matches(&member) {
                                    None
                                } else {
                                    Some(member)
                                }
                            })
                            .collect::<Vec<_>>();
                        // Clone + async closure means we can use an immutable borrow.
                        let results = sender
                            .send_message_to_group(&members, None, content, timestamp, online)
                            .await;
                        for result in results {
                            if let Err(e) = result {
                                anyhow::bail!("Error sending message: {}", e);
                            }
                        }
                    }
                    orm::SessionType::DirectMessage(recipient) => {
                        let recipient = recipient.to_service_address();

                        if let Err(e) = sender
                            .send_message(&recipient, None, content.clone(), timestamp, online)
                            .await
                        {
                            anyhow::bail!("Error sending message: {}", e);
                        }
                    }
                }

                // Mark as sent
                storage.dequeue_message(mid, chrono::Utc::now().naive_utc());

                Ok((session.id, mid, msg.text))
            }
            .into_actor(self)
            .map(move |res, act, _ctx| match res {
                Ok((sid, mid, message)) => {
                    act.inner.pinned().borrow().messageSent(
                        sid,
                        mid,
                        message.unwrap_or_else(String::new).into(),
                    );

                    actix::spawn(
                        act.inner
                            .pinned()
                            .borrow()
                            .session_actor
                            .clone()
                            .unwrap()
                            .send(LoadAllSessions)
                            .map(Result::unwrap),
                    );
                }
                Err(e) => log::error!("Sending message: {}", e),
            }),
        )
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

    fn handle(&mut self, storageready: StorageReady, _ctx: &mut Self::Context) -> Self::Result {
        self.storage = Some(storageready.storage.clone());
        let tel = self.config.get_tel_clone();
        let uuid = self.config.get_uuid_clone();

        let storage_for_password = storageready.storage;
        let request_password = async move {
            // Web socket
            let phonenumber = phonenumber::parse(None, tel).unwrap();
            let uuid = if !uuid.is_empty() {
                match uuid::Uuid::parse_str(&uuid) {
                    Ok(uuid) => Some(uuid),
                    Err(e) => {
                        log::error!("Could not parse uuid {}. Try removing the uuid field in config.yaml and restart Whisperfish. {}", &uuid, e);
                        None
                    }
                }
            } else {
                None
            };
            log::info!("Phone number: {}", phonenumber);
            log::info!("UUID: {:?}", uuid);

            let password = storage_for_password.signal_password().await.unwrap();
            let signaling_key = Some(storage_for_password.signaling_key().await.unwrap());

            (uuid, phonenumber, password, signaling_key)
        };
        let service_cfg = self.service_cfg();

        Box::pin(request_password.into_actor(self).map(
            move |(uuid, phonenumber, password, signaling_key), act, ctx| {
                // Store credentials
                let credentials = ServiceCredentials {
                    uuid,
                    phonenumber: phonenumber.clone(),
                    password: Some(password),
                    signaling_key,
                    device_id: None, // !77
                };
                act.credentials = Some(credentials);
                // end store credentials

                // Signal service context
                let local_addr = ServiceAddress {
                    uuid,
                    phonenumber: Some(phonenumber),
                    relay: None,
                };
                let storage = act.storage.clone().unwrap();
                let cipher = ServiceCipher::new(
                    storage.clone(),
                    storage.clone(),
                    storage.clone(),
                    storage,
                    rand::thread_rng(),
                    service_cfg.credentials_validator().expect("trust root"),
                );
                // end signal service context
                act.cipher = Some(cipher);
                act.local_addr = Some(local_addr);

                ctx.notify(Restart);

                ctx.notify(RefreshPreKeys);

                ctx.notify(Migrations);
            },
        ))
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct Restart;

impl Handler<Restart> for ClientActor {
    type Result = ResponseActFuture<Self, ()>;

    fn handle(&mut self, _: Restart, _ctx: &mut Self::Context) -> Self::Result {
        let service = self.authenticated_service();
        let credentials = self.credentials.clone().unwrap();

        self.inner.pinned().borrow_mut().connected = false;
        self.inner.pinned().borrow().connectedChanged();
        Box::pin(
            async move {
                let mut receiver = MessageReceiver::new(service.clone());

                receiver.create_message_pipe(credentials).await
            }
            .into_actor(self)
            .map(move |pipe, act, ctx| match pipe {
                Ok(pipe) => {
                    ctx.add_stream(pipe.stream());
                    act.inner.pinned().borrow_mut().connected = true;
                    act.inner.pinned().borrow().connectedChanged();
                }
                Err(e) => {
                    log::error!("Error starting stream: {}", e);
                    log::info!("Retrying in 10");
                    let addr = ctx.address();
                    actix::spawn(async move {
                        actix::clock::sleep(Duration::from_secs(10)).await;
                        addr.send(Restart).await.expect("retry restart");
                    });
                }
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

        let mut cipher = self.cipher.clone().expect("cipher initialized");

        if msg.is_prekey_signal_message()
            || msg.is_signal_message()
            || msg.is_unidentified_sender()
            || msg.is_receipt()
        {
            ctx.spawn(
                async move {
                    let content = match cipher.open_envelope(msg).await {
                        Ok(Some(content)) => content,
                        Ok(None) => {
                            log::info!("Empty envelope");
                            return None;
                        }
                        Err(e) => {
                            log::error!("Error opening envelope: {:?}", e);
                            return None;
                        }
                    };

                    log::trace!("Opened envelope: {:?}", content);

                    Some(content)
                }
                .into_actor(self)
                .map(|content, act, ctx| {
                    if let Some(content) = content {
                        act.process_envelope(content, ctx);
                    }
                }),
            );
        } else {
            log::warn!("Unknown envelope type {:?}", msg.r#type());
        }
    }

    /// Called when the WebSocket somehow has disconnected.
    fn finished(&mut self, ctx: &mut Self::Context) {
        log::debug!("Attempting reconnect");

        self.inner.pinned().borrow_mut().connected = false;
        self.inner.pinned().borrow().connectedChanged();

        ctx.notify(Restart);
    }
}

#[derive(Message)]
#[rtype(result = "Result<VerificationCodeResponse, anyhow::Error>")]
pub struct Register {
    pub phonenumber: PhoneNumber,
    pub password: String,
    pub use_voice: bool,
    pub captcha: Option<String>,
}

impl Handler<Register> for ClientActor {
    type Result = ResponseActFuture<Self, Result<VerificationCodeResponse, anyhow::Error>>;

    fn handle(&mut self, reg: Register, _ctx: &mut Self::Context) -> Self::Result {
        let Register {
            phonenumber,
            password,
            use_voice,
            captcha,
        } = reg;

        let mut push_service = self.authenticated_service_with_credentials(ServiceCredentials {
            uuid: None,
            phonenumber: phonenumber.clone(),
            password: Some(password.clone()),
            signaling_key: None,
            device_id: None, // !77
        });
        // XXX add profile key when #192 implemneted
        let registration_procedure = async move {
            let mut provisioning_manager = ProvisioningManager::<AwcPushService>::new(
                &mut push_service,
                phonenumber,
                password,
            );
            if use_voice {
                provisioning_manager
                    .request_voice_verification_code(captcha.as_deref(), None)
                    .await
                    .map(Into::into)
            } else {
                provisioning_manager
                    .request_sms_verification_code(captcha.as_deref(), None)
                    .await
                    .map(Into::into)
            }
        };

        Box::pin(
            registration_procedure
                .into_actor(self)
                .map(|result, _act, _ctx| Ok(result?)),
        )
    }
}

#[derive(Message)]
#[rtype(result = "Result<(u32, ConfirmCodeResponse), anyhow::Error>")]
pub struct ConfirmRegistration {
    pub phonenumber: PhoneNumber,
    pub password: String,
    pub confirm_code: u32,
    pub signaling_key: [u8; 52],
}

impl Handler<ConfirmRegistration> for ClientActor {
    type Result = ResponseActFuture<Self, Result<(u32, ConfirmCodeResponse), anyhow::Error>>;

    fn handle(&mut self, confirm: ConfirmRegistration, _ctx: &mut Self::Context) -> Self::Result {
        use libsignal_service::provisioning::*;

        let ConfirmRegistration {
            phonenumber,
            password,
            confirm_code,
            signaling_key,
        } = confirm;

        let registration_id = generate_registration_id(&mut rand::thread_rng());
        log::trace!("registration_id: {}", registration_id);

        let mut push_service = self.authenticated_service_with_credentials(ServiceCredentials {
            uuid: None,
            phonenumber: phonenumber.clone(),
            password: Some(password.clone()),
            signaling_key: None,
            device_id: None, // !77
        });
        let confirmation_procedure = async move {
            let mut provisioning_manager = ProvisioningManager::<AwcPushService>::new(
                &mut push_service,
                phonenumber,
                password,
            );
            provisioning_manager
                .confirm_verification_code(
                    confirm_code,
                    ConfirmCodeMessage::new_without_unidentified_access(
                        signaling_key.to_vec(),
                        registration_id,
                    ),
                )
                .await
        };

        Box::pin(
            confirmation_procedure
                .into_actor(self)
                .map(move |result, _act, _ctx| Ok((registration_id, result?))),
        )
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct RefreshPreKeys;

/// Java's RefreshPreKeysJob
impl Handler<RefreshPreKeys> for ClientActor {
    type Result = ResponseActFuture<Self, ()>;

    fn handle(&mut self, _: RefreshPreKeys, _ctx: &mut Self::Context) -> Self::Result {
        log::trace!("handle(RefreshPreKeys)");

        let service = self.authenticated_service();
        // XXX add profile key when #192 implemneted
        let mut am = AccountManager::new(service, None);
        let storage = self.storage.clone().unwrap();

        let proc = async move {
            let (next_signed_pre_key_id, pre_keys_offset_id) = storage.next_pre_key_ids().await;

            am.update_pre_key_bundle(
                &storage.clone(),
                &mut storage.clone(),
                &mut storage.clone(),
                &mut rand::thread_rng(),
                next_signed_pre_key_id,
                pre_keys_offset_id,
                false,
            )
            .await
        };
        // XXX: store the last refresh time somewhere.

        Box::pin(proc.into_actor(self).map(move |result, _act, _ctx| {
            if let Err(e) = result {
                log::error!("Refresh pre keys failed: {}", e);
            } else {
                log::trace!("Successfully refreshed prekeys");
            }
        }))
    }
}

// methods called from Qt
impl ClientWorker {
    pub fn compress_db(&self) {
        let actor = self.actor.clone().unwrap();
        actix::spawn(async move {
            if let Err(e) = actor.send(CompactDb(0)).await {
                log::error!("{:?}", e);
            }
        });
    }
}

impl Handler<CompactDb> for ClientActor {
    type Result = usize;

    fn handle(&mut self, _: CompactDb, _ctx: &mut Self::Context) -> Self::Result {
        log::trace!("handle(CompactDb)");
        let store = self.storage.clone().unwrap();
        let res = store.compress_db();
        log::trace!("  res = {}", res);
        res
    }
}
