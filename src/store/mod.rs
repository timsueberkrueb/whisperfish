use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use parking_lot::ReentrantMutex;

use crate::schema;
use crate::settings::SignalConfig;

use chrono::prelude::*;
use diesel::debug_query;
use diesel::prelude::*;

use futures::io::AsyncRead;

use failure::*;

mod protocol_store;
use protocol_store::ProtocolStore;

pub mod orm;

embed_migrations!();

no_arg_sql_function!(
    last_insert_rowid,
    diesel::sql_types::Integer,
    "Represents the Sqlite last_insert_rowid() function"
);

/// Session as it relates to the schema
#[derive(Queryable, Debug, Clone)]
pub struct Session {
    pub id: i32,
    pub source: String,
    pub message: String,
    pub timestamp: NaiveDateTime,
    pub sent: bool,
    pub received: bool,
    pub unread: bool,
    pub is_group: bool,
    pub group_members: Option<String>,
    #[allow(dead_code)]
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    pub has_attachment: bool,
}

/// Message as it relates to the schema
#[derive(Queryable, Debug)]
pub struct Message {
    pub id: i32,
    pub sid: i32,
    pub source: String,
    pub message: String, // NOTE: "text" in schema, doesn't apparently matter
    pub timestamp: NaiveDateTime,
    pub sent: bool,
    pub received: bool,
    pub flags: i32,
    pub attachment: Option<String>,
    pub mimetype: Option<String>,
    pub hasattachment: bool,
    pub outgoing: bool,
    pub queued: bool,
}

/// ID-free Message model for insertions
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub session_id: Option<i32>,
    pub source: String,
    pub text: String,
    pub timestamp: NaiveDateTime,
    pub sent: bool,
    pub received: bool,
    pub is_read: bool,
    pub flags: i32,
    pub attachment: Option<String>,
    pub mime_type: Option<String>,
    pub has_attachment: bool,
    pub outgoing: bool,
}

/// ID-free Group model for insertions
#[derive(Clone, Debug)]
pub struct NewGroupV1<'a> {
    pub id: &'a [u8],
    /// Group name
    pub name: String,
    /// List of E164
    pub members: Vec<String>,
}

/// Saves a given attachment into a random-generated path. Returns the path.
///
/// This was a Message method in Go
pub async fn save_attachment(
    dir: impl AsRef<Path>,
    ext: &str,
    mut attachment: impl AsyncRead + Unpin,
) -> PathBuf {
    use std::fs::File;
    use uuid::Uuid;

    let fname = Uuid::new_v4().to_simple();
    let fname_formatted = format!("{}", fname);
    let fname_path = Path::new(&fname_formatted);

    let mut path = dir.as_ref().join(fname_path);
    path.set_extension(ext);

    let file = File::create(&path).expect("Could not create file");

    // https://github.com/rust-lang/futures-rs/issues/2105
    // https://github.com/tokio-rs/tokio/pull/1744
    let mut file = futures::io::AllowStdIo::new(file);
    futures::io::copy(&mut attachment, &mut file).await.unwrap();

    path
}

/// Location of the storage.
///
/// Path is for persistent storage.
/// Memory is for running tests or 'incognito' mode.
#[cfg_attr(not(test), allow(unused))]
pub enum StorageLocation<P> {
    Path(P),
    Memory,
}

impl<'a> From<&'a Path> for StorageLocation<&'a Path> {
    fn from(p: &'a Path) -> Self {
        StorageLocation::Path(p)
    }
}

impl From<PathBuf> for StorageLocation<PathBuf> {
    fn from(p: PathBuf) -> Self {
        StorageLocation::Path(p)
    }
}

#[cfg_attr(not(test), allow(unused))]
pub fn memory() -> StorageLocation<PathBuf> {
    StorageLocation::Memory
}

#[cfg_attr(not(test), allow(unused))]
#[cfg(unix)]
pub fn temp() -> StorageLocation<tempdir::TempDir> {
    StorageLocation::Path(tempdir::TempDir::new("harbour-whisperfish-temp").unwrap())
}

pub fn default_location() -> Result<StorageLocation<PathBuf>, Error> {
    let data_dir =
        dirs::data_local_dir().ok_or_else(|| format_err!("Could not find data directory."))?;

    Ok(StorageLocation::Path(data_dir.join("harbour-whisperfish")))
}

impl<P: AsRef<Path>> std::ops::Deref for StorageLocation<P> {
    type Target = Path;
    fn deref(&self) -> &Path {
        match self {
            StorageLocation::Memory => unimplemented!(":memory: deref"),
            StorageLocation::Path(p) => p.as_ref(),
        }
    }
}

impl<P: AsRef<Path>> StorageLocation<P> {
    fn open_db(&self) -> Result<SqliteConnection, Error> {
        let database_url = match self {
            StorageLocation::Memory => ":memory:".into(),
            StorageLocation::Path(p) => p
                .as_ref()
                .join("db")
                .join("harbour-whisperfish.db")
                .to_str()
                .ok_or_else(|| {
                    format_err!("path to db contains a non-UTF8 character, please file a bug.")
                })?
                .to_string(),
        };

        Ok(SqliteConnection::establish(&database_url)?)
    }
}

#[derive(Clone)]
pub struct Storage {
    pub db: Arc<AssertUnwindSafe<ReentrantMutex<SqliteConnection>>>,
    // aesKey + macKey
    keys: Option<[u8; 16 + 20]>,
    protocol_store: Arc<Mutex<ProtocolStore>>,
    path: PathBuf,
}

// Cannot borrow password/salt because threadpool requires 'static...
async fn derive_storage_key(password: String, salt_path: PathBuf) -> Result<[u8; 16 + 20], Error> {
    use actix_threadpool::BlockingError;
    use std::io::Read;

    actix_threadpool::run(move || -> Result<_, failure::Error> {
        let mut salt_file = std::fs::File::open(salt_path)?;
        let mut salt = [0u8; 8];
        ensure!(salt_file.read(&mut salt)? == 8, "salt file not 8 bytes");

        let mut key = [0u8; 16 + 20];
        // Please don't blame me, I'm only the implementer.
        pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password.as_bytes(), &salt, 1024, &mut key);
        log::trace!("Computed the key, salt was {:?}", salt);

        Ok(key)
    })
    .await
    .map_err(|e| match e {
        BlockingError::Canceled => format_err!("Threadpool Canceled"),
        BlockingError::Error(e) => e,
    })
}

// Cannot borrow password/salt because threadpool requires 'static...
async fn derive_db_key(password: String, salt_path: PathBuf) -> Result<[u8; 32], Error> {
    use actix_threadpool::BlockingError;
    use std::io::Read;

    actix_threadpool::run(move || -> Result<_, failure::Error> {
        let mut salt_file = std::fs::File::open(salt_path)?;
        let mut salt = [0u8; 8];
        ensure!(salt_file.read(&mut salt)? == 8, "salt file not 8 bytes");

        let params = scrypt::Params::new(14, 8, 1)?;
        let mut key = [0u8; 32];
        scrypt::scrypt(password.as_bytes(), &salt, &params, &mut key)?;
        log::trace!("Computed the key, salt was {:?}", salt);
        Ok(key)
    })
    .await
    .map_err(|e| match e {
        BlockingError::Canceled => format_err!("Threadpool Canceled"),
        BlockingError::Error(e) => e,
    })
}

fn write_file_sync_unencrypted(path: PathBuf, contents: &[u8]) -> Result<(), Error> {
    log::trace!("Writing unencrypted file {:?}", path);

    use std::io::Write;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(&contents)?;

    Ok(())
}

fn write_file_sync_encrypted(
    keys: [u8; 16 + 20],
    path: PathBuf,
    contents: &[u8],
) -> Result<(), Error> {
    log::trace!("Writing encrypted file {:?}", path);

    // Generate random IV
    use rand::RngCore;
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);

    // Encrypt
    use aes::Aes128;
    use block_modes::block_padding::Pkcs7;
    use block_modes::{BlockMode, Cbc};
    let ciphertext = {
        let cipher = Cbc::<Aes128, Pkcs7>::new_var(&keys[0..16], &iv)
            .map_err(|_| format_err!("CBC initialization error"))?;
        cipher.encrypt_vec(contents)
    };

    let mac = {
        use hmac::{Hmac, Mac, NewMac};
        use sha2::Sha256;
        // Verify HMAC SHA256, 32 last bytes
        let mut mac = Hmac::<Sha256>::new_varkey(&keys[16..])
            .map_err(|_| format_err!("MAC keylength error"))?;
        mac.update(&iv);
        mac.update(&ciphertext);
        mac.finalize().into_bytes()
    };

    // Write iv, ciphertext, mac
    use std::io::Write;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(&iv)?;
    file.write_all(&ciphertext)?;
    file.write_all(&mac)?;

    Ok(())
}

fn write_file_sync(
    keys: Option<[u8; 16 + 20]>,
    path: PathBuf,
    contents: &[u8],
) -> Result<(), Error> {
    match keys {
        Some(keys) => write_file_sync_encrypted(keys, path, &contents),
        None => write_file_sync_unencrypted(path, &contents),
    }
}

async fn write_file(
    keys: Option<[u8; 16 + 20]>,
    path: PathBuf,
    contents: Vec<u8>,
) -> Result<(), Error> {
    actix_threadpool::run(move || write_file_sync(keys, path, &contents)).await?;
    Ok(())
}

fn load_file_sync_unencrypted(path: PathBuf) -> Result<Vec<u8>, Error> {
    log::trace!("Opening unencrypted file {:?}", path);
    let contents = std::fs::read(&path)?;
    let count = contents.len();
    log::trace!("Read {:?}, {} bytes", path, count);
    Ok(contents)
}

fn load_file_sync_encrypted(keys: [u8; 16 + 20], path: PathBuf) -> Result<Vec<u8>, Error> {
    // XXX This is *full* of bad practices.
    // Let's try to migrate to nacl or something alike in the future.

    log::trace!("Opening encrypted file {:?}", path);
    let mut contents = std::fs::read(&path)?;
    let count = contents.len();

    log::trace!("Read {:?}, {} bytes", path, count);
    ensure!(count >= 16 + 32, "File smaller than cryptographic overhead");

    let (iv, contents) = contents.split_at_mut(16);
    let count = count - 16;
    let (contents, mac) = contents.split_at_mut(count - 32);

    {
        use hmac::{Hmac, Mac, NewMac};
        use sha2::Sha256;
        // Verify HMAC SHA256, 32 last bytes
        let mut verifier = Hmac::<Sha256>::new_varkey(&keys[16..])
            .map_err(|_| format_err!("MAC keylength error"))?;
        verifier.update(&iv);
        verifier.update(contents);
        verifier
            .verify(&mac)
            .map_err(|_| format_err!("MAC error"))?;
    }

    use aes::Aes128;
    use block_modes::block_padding::Pkcs7;
    use block_modes::{BlockMode, Cbc};
    // Decrypt password
    let cipher = Cbc::<Aes128, Pkcs7>::new_var(&keys[0..16], &iv)
        .map_err(|_| format_err!("CBC initialization error"))?;
    Ok(cipher
        .decrypt(contents)
        .map_err(|_| format_err!("AES CBC decryption error"))?
        .to_owned())
}

fn load_file_sync(keys: Option<[u8; 16 + 20]>, path: PathBuf) -> Result<Vec<u8>, Error> {
    match keys {
        Some(keys) => load_file_sync_encrypted(keys, path),
        None => load_file_sync_unencrypted(path),
    }
}

async fn load_file(keys: Option<[u8; 16 + 20]>, path: PathBuf) -> Result<Vec<u8>, Error> {
    let contents = actix_threadpool::run(move || load_file_sync(keys, path)).await?;

    Ok(contents)
}

/// Fetches an `orm::Session`, for which the supplied closure can impose constraints.
///
/// This *can* in principe be implemented with pure type constraints,
/// but I'm not in the mood for digging a few hours through Diesel's traits.
macro_rules! fetch_session {
    ($db:expr, |$fragment:ident| $b:block ) => {{
        let db = $db;
        let query = {
            let $fragment = schema::sessions::table
                .left_join(schema::recipients::table)
                .left_join(schema::group_v1s::table);
            $b
        };
        let triple: Option<(orm::DbSession, Option<orm::Recipient>, Option<orm::GroupV1>)> =
            query.first(&*db).ok();
        triple.map(Into::into)
    }};
}
macro_rules! fetch_sessions {
    ($db:expr, |$fragment:ident| $b:block ) => {{
        let db = $db;
        let query = {
            let $fragment = schema::sessions::table
                .left_join(schema::recipients::table)
                .left_join(schema::group_v1s::table);
            $b
        };
        let triples: Vec<(orm::DbSession, Option<orm::Recipient>, Option<orm::GroupV1>)> =
            query.load(&*db).unwrap();
        triples.into_iter().map(orm::Session::from).collect()
    }};
}

impl Storage {
    /// Returns the path to the storage.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_config(&self) -> Result<SignalConfig, Error> {
        let signal_config_file = crate::conf_dir().join("config.yml");
        let file = std::fs::File::open(&signal_config_file)?;
        Ok(serde_yaml::from_reader(file)?)
    }

    pub fn write_config(&self, cfg: SignalConfig) -> Result<(), Error> {
        let signal_config_file = crate::conf_dir().join("config.yml");
        let file = std::fs::File::create(signal_config_file)?;
        serde_yaml::to_writer(file, &cfg)?;
        Ok(())
    }

    fn scaffold_directories(root: impl AsRef<Path>) -> Result<(), Error> {
        let root = root.as_ref();

        let directories = [
            root.to_path_buf() as PathBuf,
            root.join("db"),
            root.join("storage"),
            root.join("storage").join("identity"),
            root.join("storage").join("attachments"),
            root.join("storage").join("sessions"),
            root.join("storage").join("prekeys"),
            root.join("storage").join("signed_prekeys"),
            root.join("storage").join("groups"),
        ];

        for dir in &directories {
            if dir.exists() {
                if dir.is_dir() {
                    continue;
                } else {
                    failure::bail!(
                        "Trying to create directory {:?}, but already exists as non-directory.",
                        dir
                    );
                }
            }
            std::fs::create_dir(dir)?;
        }
        Ok(())
    }

    /// Writes (*overwrites*) a new Storage object to the provided path.
    pub async fn new<T: AsRef<Path>>(
        db_path: &StorageLocation<T>,
        password: Option<&str>,
        regid: u32,
        http_password: &str,
        signaling_key: [u8; 52],
    ) -> Result<Storage, Error> {
        let path: &Path = std::ops::Deref::deref(db_path);

        log::info!("Creating directory structure");
        Self::scaffold_directories(path)?;

        // 1. Generate both salts if needed
        let storage_salt_path = path.join("storage").join("salt");
        if password != None {
            let db_salt_path = path.join("db").join("salt");

            use rand::RngCore;
            log::info!("Generating salts");
            let mut db_salt = [0u8; 8];
            let mut storage_salt = [0u8; 8];
            let mut rng = rand::thread_rng();
            rng.fill_bytes(&mut db_salt);
            rng.fill_bytes(&mut storage_salt);

            std::fs::write(&db_salt_path, db_salt)?;
            std::fs::write(&storage_salt_path, storage_salt)?;
        }

        // 2. Open DB
        let db = Self::open_db(&db_path, &path, password).await?;

        // 3. initialize protocol store
        let keys = match password {
            None => None,
            Some(pass) => Some(derive_storage_key(pass.to_string(), storage_salt_path).await?),
        };

        let context = libsignal_protocol::Context::default();
        let identity_key_pair = libsignal_protocol::generate_identity_key_pair(&context).unwrap();

        let protocol_store =
            ProtocolStore::store_with_key(keys, path, regid, identity_key_pair).await?;

        // 4. save http password and signaling key
        let identity_path = path.join("storage").join("identity");
        write_file(
            keys,
            identity_path.join("http_password"),
            http_password.as_bytes().into(),
        )
        .await?;
        write_file(
            keys,
            identity_path.join("http_signaling_key"),
            signaling_key.to_vec(),
        )
        .await?;

        Ok(Storage {
            db: Arc::new(AssertUnwindSafe(ReentrantMutex::new(db))),
            keys,
            protocol_store: Arc::new(Mutex::new(protocol_store)),
            path: path.to_path_buf(),
        })
    }

    pub async fn open<T: AsRef<Path>>(
        db_path: &StorageLocation<T>,
        password: Option<String>,
    ) -> Result<Storage, Error> {
        let path: &Path = std::ops::Deref::deref(db_path);

        let db = Self::open_db(&db_path, &path, password.as_deref()).await?;

        let keys = match password {
            None => None,
            Some(pass) => {
                let salt_path = path.join("storage").join("salt");
                Some(derive_storage_key(pass, salt_path).await?)
            }
        };

        let protocol_store = ProtocolStore::open_with_key(keys, path).await?;

        Ok(Storage {
            db: Arc::new(AssertUnwindSafe(ReentrantMutex::new(db))),
            keys,
            protocol_store: Arc::new(Mutex::new(protocol_store)),
            path: path.to_path_buf(),
        })
    }

    async fn open_db<T: AsRef<Path>>(
        db_path: &StorageLocation<T>,
        path: &Path,
        password: Option<&str>,
    ) -> Result<SqliteConnection, Error> {
        log::info!("Opening DB");
        let db = db_path.open_db()?;

        if password != None {
            log::info!("Setting DB encryption");

            let db_salt_path = path.join("db").join("salt");
            let db_key = derive_db_key(password.unwrap().to_string(), db_salt_path);

            db.execute(&format!(
                "PRAGMA key = \"x'{}'\";",
                hex::encode(db_key.await?)
            ))?;
            db.execute("PRAGMA cipher_page_size = 4096;")?;
        }

        // From the sqlcipher manual:
        // -- if this throws an error, the key was incorrect. If it succeeds and returns a numeric value, the key is correct;
        db.execute("SELECT count(*) FROM sqlite_master;")?;
        // XXX: Do we have to signal somehow that the password was wrong?
        //      Offer retries?

        // Run migrations
        embedded_migrations::run(&db)?;

        Ok(db)
    }

    /// Asynchronously loads the signal HTTP password from storage and decrypts it.
    pub async fn signal_password(&self) -> Result<String, Error> {
        let contents = self
            .load_file(
                self.path
                    .join("storage")
                    .join("identity")
                    .join("http_password"),
            )
            .await?;
        Ok(String::from_utf8(contents)?)
    }

    /// Asynchronously loads the base64 encoded signaling key.
    pub async fn signaling_key(&self) -> Result<[u8; 52], Error> {
        let v = self
            .load_file(
                self.path
                    .join("storage")
                    .join("identity")
                    .join("http_signaling_key"),
            )
            .await?;
        ensure!(v.len() == 52, "Signaling key is 52 bytes");
        let mut out = [0u8; 52];
        out.copy_from_slice(&v);
        Ok(out)
    }

    async fn load_file(&self, path: PathBuf) -> Result<Vec<u8>, Error> {
        load_file(self.keys, path).await
    }

    /// Process message and store in database and update or create a session.
    pub fn process_message(
        &self,
        mut new_message: NewMessage,
        group: Option<NewGroupV1<'_>>,
    ) -> (orm::Message, orm::Session) {
        let session = if let Some(group) = group.as_ref() {
            self.fetch_or_insert_session_by_group_v1(group)
        } else {
            self.fetch_or_insert_session_by_e164(&new_message.source)
        };

        new_message.session_id = Some(session.id);
        let message = self.create_message(&new_message);
        (message, session)
    }

    pub fn fetch_recipient_by_e164(&self, new_e164: &str) -> Option<orm::Recipient> {
        use crate::schema::recipients::dsl::*;

        let db = self.db.lock();

        recipients.filter(e164.eq(new_e164)).first(&*db).ok()
    }

    pub fn fetch_or_insert_recipient_by_e164(&self, new_e164: &str) -> orm::Recipient {
        use crate::schema::recipients::dsl::*;

        let db = self.db.lock();

        if let Some(recipient) = recipients.filter(e164.eq(new_e164)).first(&*db).ok() {
            recipient
        } else {
            diesel::insert_into(recipients)
                .values(e164.eq(new_e164))
                .execute(&*db)
                .expect("insert new recipient");
            recipients
                .filter(e164.eq(new_e164))
                .first(&*db)
                .expect("newly inserted recipient")
        }
    }

    pub fn fetch_last_message_by_session_id(&self, sid: i32) -> Option<orm::Message> {
        use schema::messages::dsl::*;
        let db = self.db.lock();

        messages
            .filter(session_id.eq(sid))
            .order_by(server_timestamp.desc())
            .first(&*db)
            .ok()
    }

    pub fn fetch_message_receipts(&self, mid: i32) -> Vec<(orm::Receipt, orm::Recipient)> {
        use schema::{receipts, recipients};
        let db = self.db.lock();

        receipts::table
            .inner_join(recipients::table)
            .filter(receipts::message_id.eq(mid))
            .load(&*db)
            .expect("db")
    }

    /// Marks the message with a certain timestamp as read by a certain person.
    ///
    /// This is e.g. called from Signal Desktop from a sync message
    pub fn mark_message_read(
        &self,
        timestamp: NaiveDateTime,
    ) -> Option<(orm::Session, orm::Message)> {
        let db = self.db.lock();

        use schema::messages::dsl::*;
        diesel::update(messages)
            .filter(server_timestamp.eq(timestamp))
            .set(is_read.eq(true))
            .execute(&*db)
            .unwrap();

        let message: Option<orm::Message> = messages
            .filter(server_timestamp.eq(timestamp))
            .first(&*db)
            .ok();
        if let Some(message) = message {
            let session = self
                .fetch_session_by_id(message.session_id)
                .expect("foreignk key");
            Some((session, message))
        } else {
            None
        }
    }

    /// Marks the message with a certain timestamp as received by a certain person.
    ///
    /// Copy from Go's MarkMessageReceived.
    pub fn mark_message_received(
        &self,
        receiver_e164: Option<&str>,
        receiver_uuid: Option<&str>,
        timestamp: NaiveDateTime,
    ) -> Option<(orm::Session, orm::Message)> {
        let db = self.db.lock();

        // XXX: probably, the trigger for this method call knows a better time stamp.
        let time = chrono::Utc::now().naive_utc();

        // Find the recipient
        let recipient_id: i32 = schema::recipients::table
            .select(schema::recipients::id)
            .filter(
                schema::recipients::e164
                    .eq(receiver_e164)
                    .and(schema::recipients::e164.is_not_null())
                    .or(schema::recipients::uuid
                        .eq(receiver_uuid)
                        .and(schema::recipients::uuid.is_not_null())),
            )
            .first(&*db)
            .ok()?; // ? -> if unknown recipient, do not insert read status.
        let message_id = schema::messages::table
            .select(schema::messages::id)
            .filter(schema::messages::server_timestamp.eq(timestamp))
            .first(&*db)
            .ok();
        if message_id.is_none() {
            log::warn!("Could not find message with timestamp {}", timestamp);
            log::warn!(
                "This probably indicates out-of-order receipt delivery. Please upvote issue #260"
            );
        }
        let message_id = message_id?;

        let insert = diesel::insert_into(schema::receipts::table)
            .values((
                schema::receipts::message_id.eq(message_id),
                schema::receipts::recipient_id.eq(recipient_id),
                schema::receipts::delivered.eq(time),
            ))
            // UPSERT in Diesel 2.0
            // .on_conflict((schema::receipts::message_id, schema::receipts::recipient_id))
            // .do_update()
            // .set(delivered.eq(time))
            .execute(&*db);

        use diesel::result::DatabaseErrorKind;
        use diesel::result::Error::DatabaseError;
        match insert {
            Ok(1) => {
                let message = self.fetch_message_by_id(message_id)?;
                let session = self.fetch_session_by_id(message.session_id)?;
                return Some((session, message));
            }
            Ok(affected_rows) => {
                // Reason can be a dupe receipt (=0).
                log::warn!(
                    "Read receipt had {} affected rows instead of expected 1.  Ignoring.",
                    affected_rows
                );
            }
            Err(DatabaseError(DatabaseErrorKind::UniqueViolation, _)) => {
                log::trace!("receipt already exists, updating record");
            }
            Err(e) => {
                log::error!("Could not insert receipt: {}. Continuing", e);
                return None;
            }
        }
        // As of here, insertion failed because of conflict. Use update instead (issue #101 for
        // upsert).
        let update = diesel::update(schema::receipts::table)
            .filter(schema::receipts::message_id.eq(message_id))
            .filter(schema::receipts::recipient_id.eq(recipient_id))
            .set((schema::receipts::delivered.eq(time),))
            .execute(&*db);
        if let Err(e) = update {
            log::error!("Could not update receipt: {}", e);
        }
        insert.ok();

        let message = self.fetch_message_by_id(message_id)?;
        let session = self.fetch_session_by_id(message.session_id)?;

        Some((session, message))
    }

    /// Fetches the latest session by last_insert_rowid.
    ///
    /// This only yields correct results when the last insertion was in fact a session.
    fn fetch_latest_session(&self) -> Option<orm::Session> {
        fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::id.eq(last_insert_rowid))
        })
    }

    /// Get all sessions in no particular order.
    ///
    /// Getting them ordered by timestamp would be nice,
    /// but that requires table aliases or complex subqueries,
    /// which are not really a thing in Diesel atm.
    pub fn fetch_sessions(&self) -> Vec<orm::Session> {
        fetch_sessions!(self.db.lock(), |query| { query })
    }

    pub fn fetch_group_sessions(&self) -> Vec<orm::Session> {
        fetch_sessions!(self.db.lock(), |query| {
            query.filter(schema::sessions::group_v1_id.is_not_null())
        })
    }

    pub fn fetch_session_by_id(&self, sid: i32) -> Option<orm::Session> {
        fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::id.eq(sid))
        })
    }

    pub fn fetch_session_by_e164(&self, e164: &str) -> Option<orm::Session> {
        log::trace!("Called fetch__session_by_e164({})", e164);
        let db = self.db.lock();
        fetch_session!(db, |query| {
            query.filter(schema::recipients::e164.eq(e164))
        })
    }

    pub fn fetch_attachments_for_message(&self, mid: i32) -> Vec<orm::Attachment> {
        use schema::attachments::dsl::*;
        let db = self.db.lock();
        attachments
            .filter(message_id.eq(mid))
            .order_by(display_order.asc())
            .load(&*db)
            .unwrap()
    }

    pub fn fetch_group_members_by_group_v1_id(
        &self,
        id: &str,
    ) -> Vec<(orm::GroupV1Member, orm::Recipient)> {
        let db = self.db.lock();
        schema::group_v1_members::table
            .inner_join(schema::recipients::table)
            .filter(schema::group_v1_members::group_v1_id.eq(id))
            .load(&*db)
            .unwrap()
    }

    pub fn fetch_or_insert_session_by_e164(&self, e164: &str) -> orm::Session {
        log::trace!("Called fetch_or_insert_session_by_e164({})", e164);
        let db = self.db.lock();
        if let Some(session) = self.fetch_session_by_e164(e164) {
            return session;
        }

        let recipient = self.fetch_or_insert_recipient_by_e164(e164);

        use schema::sessions::dsl::*;
        diesel::insert_into(sessions)
            .values((direct_message_recipient_id.eq(recipient.id),))
            .execute(&*db)
            .unwrap();

        self.fetch_latest_session()
            .expect("a session has been inserted")
    }

    pub fn fetch_or_insert_session_by_group_v1(&self, group: &NewGroupV1<'_>) -> orm::Session {
        let db = self.db.lock();

        let group_id = hex::encode(&group.id);

        log::trace!("Called fetch_or_insert_session_by_group_v1({})", group_id);

        if let Some(session) = fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::group_v1_id.eq(&group_id))
        }) {
            return session;
        }

        let new_group = orm::GroupV1 {
            id: group_id.clone(),
            name: group.name.clone(),
        };

        // Group does not exist, insert first.
        diesel::insert_into(schema::group_v1s::table)
            .values(&new_group)
            .execute(&*db)
            .unwrap();

        let now = chrono::Utc::now().naive_utc();
        for member in &group.members {
            use schema::group_v1_members::dsl::*;
            let recipient = self.fetch_or_insert_recipient_by_e164(member);

            diesel::insert_into(group_v1_members)
                .values((
                    group_v1_id.eq(&group_id),
                    recipient_id.eq(recipient.id),
                    member_since.eq(now),
                ))
                .execute(&*db)
                .unwrap();
        }

        use schema::sessions::dsl::*;
        diesel::insert_into(sessions)
            .values((group_v1_id.eq(group_id),))
            .execute(&*db)
            .unwrap();

        self.fetch_latest_session()
            .expect("a session has been inserted")
    }

    pub fn delete_session(&self, id: i32) {
        let db = self.db.lock();

        log::trace!("Called delete_session({})", id);

        let affected_rows =
            diesel::delete(schema::sessions::table.filter(schema::sessions::id.eq(id)))
                .execute(&*db)
                .expect("delete session");

        log::trace!("delete_session({}) affected {} rows", id, affected_rows);
    }

    pub fn mark_session_read(&self, sid: i32) {
        let db = self.db.lock();

        log::trace!("Called mark_session_read({})", sid);

        use schema::messages::dsl::*;

        diesel::update(messages.filter(session_id.eq(sid)))
            .set((is_read.eq(true),))
            .execute(&*db)
            .expect("mark session read");
    }

    pub fn register_attachment(&mut self, mid: i32, path: &str, mime_type: &str) {
        // XXX: multiple attachments https://gitlab.com/rubdos/whisperfish/-/issues/11

        let db = self.db.lock();

        diesel::insert_into(schema::attachments::table)
            .values((
                // XXX: many more things to store !
                schema::attachments::message_id.eq(mid),
                schema::attachments::content_type.eq(mime_type),
                schema::attachments::attachment_path.eq(path),
                schema::attachments::is_voice_note.eq(false),
                schema::attachments::is_borderless.eq(false),
                schema::attachments::is_quote.eq(false),
            ))
            .execute(&*db)
            .expect("insert attachment");
    }

    /// Create a new message. This was transparent within SaveMessage in Go.
    ///
    /// Panics is new_message.session_id is None.
    pub fn create_message(&self, new_message: &NewMessage) -> orm::Message {
        let db = self.db.lock();
        use schema::messages::dsl::*;

        let session = new_message.session_id.expect("session id");

        log::trace!("Called create_message(..) for session {}", session);

        let sender = self
            .fetch_recipient_by_e164(&new_message.source)
            .map(|r| r.id);

        let affected_rows = diesel::insert_into(messages)
            .values((
                session_id.eq(session),
                text.eq(&new_message.text),
                sender_recipient_id.eq(sender),
                received_timestamp.eq(chrono::Utc::now().naive_utc()),
                server_timestamp.eq(new_message.timestamp),
                is_read.eq(new_message.is_read),
                is_outbound.eq(new_message.outgoing),
                flags.eq(new_message.flags),
            ))
            .execute(&*db)
            .expect("inserting a message");
        // XXX newmessage could have an attachment

        assert_eq!(
            affected_rows, 1,
            "Did not insert the message. Dazed and confused."
        );

        // Then see if the message was inserted ok and what it was
        let latest_message = self.fetch_latest_message().expect("inserted message");
        assert_eq!(
            latest_message.session_id, session,
            "message insert sanity test failed"
        );

        log::trace!("Inserted message id {}", latest_message.id);
        latest_message
    }

    /// This was implicit in Go, which probably didn't use threads.
    ///
    /// It needs to be locked from the outside because sqlite sucks.
    fn fetch_latest_message(&self) -> Option<orm::Message> {
        let db = self.db.lock();

        schema::messages::table
            .filter(schema::messages::id.eq(last_insert_rowid))
            .first(&*db)
            .ok()
    }

    pub fn fetch_message_by_timestamp(&self, ts: NaiveDateTime) -> Option<orm::Message> {
        let db = self.db.lock();

        log::trace!("Called fetch_message_by_timestamp({})", ts);
        let query = schema::messages::table.filter(schema::messages::server_timestamp.eq(ts));

        let debug = debug_query::<diesel::sqlite::Sqlite, _>(&query);
        log::trace!("{}", debug.to_string());

        query.first(&*db).ok()
    }

    pub fn fetch_recipient_by_id(&self, id: i32) -> Option<orm::Recipient> {
        let db = self.db.lock();

        log::trace!("Called fetch_recipient_by_id({})", id);
        schema::recipients::table
            .filter(schema::recipients::id.eq(id))
            .first(&*db)
            .ok()
    }

    pub fn fetch_message_by_id(&self, id: i32) -> Option<orm::Message> {
        let db = self.db.lock();

        // Even a single message needs to know if it's queued to satisfy the `Message` trait
        log::trace!("Called fetch_message_by_id({})", id);
        schema::messages::table
            .filter(schema::messages::id.eq(id))
            .first(&*db)
            .ok()
    }

    /// Returns a vector of tuples of messages with their sender.
    ///
    /// When the sender is None, it is a sent message, not a received message.
    // XXX maybe this should be `Option<Vec<...>>`.
    pub fn fetch_all_messages(&self, sid: i32) -> Vec<(orm::Message, Option<orm::Recipient>)> {
        let db = self.db.lock();

        log::trace!("Called fetch_all_messages({})", sid);
        schema::messages::table
            .filter(schema::messages::session_id.eq(sid))
            .left_join(schema::recipients::table)
            // XXX: order by timestamp?
            .order_by(schema::messages::columns::id.desc())
            .load(&*db)
            .expect("database")
    }

    pub fn delete_message(&self, id: i32) -> Option<usize> {
        let db = self.db.lock();

        log::trace!("Called delete_message({})", id);

        // XXX: Assume `sentq` has nothing pending, as the Go version does
        let query = diesel::delete(schema::messages::table.filter(schema::messages::id.eq(id)));

        let debug = debug_query::<diesel::sqlite::Sqlite, _>(&query);
        log::trace!("{}", debug.to_string());

        query.execute(&*db).ok()
    }

    pub fn dequeue_message(&self, mid: i32, sent_time: NaiveDateTime) {
        let db = self.db.lock();

        diesel::update(schema::messages::table)
            .filter(schema::messages::id.eq(mid))
            .set(schema::messages::sent_timestamp.eq(sent_time))
            .execute(&*db)
            .unwrap();
    }

    /// Returns a hex-encoded peer identity
    pub fn peer_identity(&self, e164: &str) -> Result<String, failure::Error> {
        use libsignal_protocol::stores::IdentityKeyStore;
        use libsignal_protocol::Address;
        let addr = Address::new(e164, 1);
        let ident = self
            .get_identity(addr)?
            .ok_or(failure::format_err!("No such identity"))?;
        Ok(hex::encode_upper(ident.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest(
        mime_type,
        ext,
        case("video/mp4", "mp4"),
        case("image/jpg", "jpg"),
        case("image/jpeg", "jpg"),
        case("image/png", "png"),
        case("text/plain", "txt")
    )]
    #[actix_rt::test]
    async fn test_save_attachment(mime_type: &str, ext: &str) {
        use std::env;
        use std::fs;
        use std::path::Path;

        drop(mime_type); // This is used in client-worker, consider droppin g this argument.

        let dirname = env::temp_dir().to_str().expect("Temp dir fail").to_string();
        let dir = Path::new(&dirname);
        let mut contents = futures::io::Cursor::new([0u8]);
        let fname = save_attachment(dir, ext, &mut contents).await;

        let exists = Path::new(&fname).exists();

        println!("Looking for {}", fname.to_str().unwrap());
        assert!(exists);

        assert_eq!(
            fname.extension().unwrap(),
            ext,
            "{}",
            format!("{} <> {}", fname.to_str().unwrap(), ext)
        );

        fs::remove_file(fname).expect("Could not remove test case file");
    }

    #[test]
    fn encrypt_and_decrypt_file() -> Result<(), Error> {
        let contents = "The funny horse jumped over a river.";

        // Key full of ones.
        let key = [1u8; 16 + 20];
        let dir = temp();

        write_file_sync_encrypted(
            key,
            dir.join("encrypt-and-decrypt.temp"),
            contents.as_bytes(),
        )?;
        let res = load_file_sync_encrypted(key, dir.join("encrypt-and-decrypt.temp"))?;
        assert_eq!(std::str::from_utf8(&res).expect("utf8"), contents);

        Ok(())
    }

    #[actix_rt::test]
    async fn create_and_open_encrypted_storage() -> Result<(), Error> {
        let pass = "Hello, world! I'm the passphrase";
        test_create_and_open_storage(Some(pass.to_string())).await
    }

    #[actix_rt::test]
    async fn create_and_open_unencrypted_storage() -> Result<(), Error> {
        test_create_and_open_storage(None).await
    }

    async fn test_create_and_open_storage(storage_password: Option<String>) -> Result<(), Error> {
        use rand::distributions::Alphanumeric;
        use rand::{Rng, RngCore};

        env_logger::try_init().ok();

        let location = super::temp();
        let rng = rand::thread_rng();

        // Signaling password for REST API
        let password: Vec<u8> = rng.sample_iter(&Alphanumeric).take(24).collect();
        let password = std::str::from_utf8(&password)?;

        // Signaling key that decrypts the incoming Signal messages
        let mut rng = rand::thread_rng();
        let mut signaling_key = [0u8; 52];
        rng.fill_bytes(&mut signaling_key);
        let signaling_key = signaling_key;

        // Registration ID
        let regid = 12345;

        let storage = Storage::new(
            &location,
            storage_password.as_deref(),
            regid,
            &password,
            signaling_key,
        )
        .await?;

        macro_rules! tests {
            ($storage:ident) => {{
                use libsignal_protocol::stores::IdentityKeyStore;
                // TODO: assert that tables exist
                assert_eq!(password, $storage.signal_password().await?);
                assert_eq!(signaling_key, $storage.signaling_key().await?);
                assert_eq!(regid, $storage.local_registration_id()?);

                let (signed, unsigned) = $storage.next_pre_key_ids();
                // Unstarted client will have no pre-keys.
                assert_eq!(0, signed);
                assert_eq!(0, unsigned);

                Result::<_, Error>::Ok(())
            }};
        };

        tests!(storage)?;
        drop(storage);

        if storage_password.is_some() {
            assert!(
                Storage::open(&location, None).await.is_err(),
                "Storage was not encrypted"
            );
        }

        let storage = Storage::open(&location, storage_password).await?;

        tests!(storage)?;

        Ok(())
    }
}
