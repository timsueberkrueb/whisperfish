use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;

use libsignal_service::groups_v2::InMemoryCredentialsCache;
use libsignal_service::prelude::protocol::*;
use libsignal_service::prelude::*;
use parking_lot::ReentrantMutex;

use crate::schema;
use crate::{config::SignalConfig, millis_to_naive_chrono};

use chrono::prelude::*;
use diesel::debug_query;
use diesel::prelude::*;
use itertools::Itertools;

use futures::io::AsyncRead;

use zkgroup::api::groups::GroupSecretParams;

mod protocol_store;
use protocol_store::ProtocolStore;

pub mod orm;

embed_migrations!();

no_arg_sql_function!(
    last_insert_rowid,
    diesel::sql_types::Integer,
    "Represents the Sqlite last_insert_rowid() function"
);

/// How much trust you put into the correctness of the data.
#[derive(Clone, Copy, Eq, Debug, PartialEq)]
pub enum TrustLevel {
    /// Set to Certain if the supplied information is from a trusted source,
    /// such as an envelope.
    Certain,
    Uncertain,
}

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
    pub source_e164: Option<String>,
    pub source_uuid: Option<String>,
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

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum GroupContext {
    GroupV1(GroupV1),
    GroupV2(GroupV2),
}

impl From<GroupV1> for GroupContext {
    fn from(v1: GroupV1) -> GroupContext {
        GroupContext::GroupV1(v1)
    }
}

impl From<GroupV2> for GroupContext {
    fn from(v2: GroupV2) -> GroupContext {
        GroupContext::GroupV2(v2)
    }
}

/// ID-free Group model for insertions
#[derive(Clone, Debug)]
pub struct GroupV1 {
    pub id: Vec<u8>,
    /// Group name
    pub name: String,
    /// List of E164
    pub members: Vec<String>,
}

#[derive(Clone)]
pub struct GroupV2 {
    pub secret: GroupSecretParams,
    pub revision: u32,
}

impl std::fmt::Debug for GroupV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupV2")
            .field("id", &self.secret.get_group_identifier())
            .field("revision", &self.revision)
            .finish()
    }
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

pub fn default_location() -> Result<StorageLocation<PathBuf>, anyhow::Error> {
    let data_dir = dirs::data_local_dir().context("Could not find data directory.")?;

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
    fn open_db(&self) -> Result<SqliteConnection, anyhow::Error> {
        let database_url = match self {
            StorageLocation::Memory => ":memory:".into(),
            StorageLocation::Path(p) => p
                .as_ref()
                .join("db")
                .join("harbour-whisperfish.db")
                .to_str()
                .context("path to db contains a non-UTF8 character, please file a bug.")?
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
    pub(crate) protocol_store: Arc<tokio::sync::RwLock<ProtocolStore>>,
    credential_cache: Arc<Mutex<InMemoryCredentialsCache>>,
    path: PathBuf,
}

// Cannot borrow password/salt because threadpool requires 'static...
async fn derive_storage_key(
    password: String,
    salt_path: PathBuf,
) -> Result<[u8; 16 + 20], anyhow::Error> {
    use actix_threadpool::BlockingError;
    use std::io::Read;

    actix_threadpool::run(move || -> Result<_, anyhow::Error> {
        let mut salt_file = std::fs::File::open(salt_path).context("Cannot open salt file")?;
        let mut salt = [0u8; 8];
        anyhow::ensure!(salt_file.read(&mut salt)? == 8, "salt file not 8 bytes");

        let mut key = [0u8; 16 + 20];
        // Please don't blame me, I'm only the implementer.
        pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password.as_bytes(), &salt, 1024, &mut key);
        log::trace!("Computed the key, salt was {:?}", salt);

        Ok(key)
    })
    .await
    .map_err(|e| match e {
        BlockingError::Canceled => anyhow::anyhow!("Threadpool Canceled"),
        BlockingError::Error(e) => e,
    })
}

// Cannot borrow password/salt because threadpool requires 'static...
async fn derive_db_key(password: String, salt_path: PathBuf) -> Result<[u8; 32], anyhow::Error> {
    use actix_threadpool::BlockingError;
    use std::io::Read;

    actix_threadpool::run(move || -> Result<_, anyhow::Error> {
        let mut salt_file = std::fs::File::open(salt_path)?;
        let mut salt = [0u8; 8];
        anyhow::ensure!(salt_file.read(&mut salt)? == 8, "salt file not 8 bytes");

        let params = scrypt::Params::new(14, 8, 1)?;
        let mut key = [0u8; 32];
        scrypt::scrypt(password.as_bytes(), &salt, &params, &mut key)?;
        log::trace!("Computed the key, salt was {:?}", salt);
        Ok(key)
    })
    .await
    .map_err(|e| match e {
        BlockingError::Canceled => anyhow::anyhow!("Threadpool Canceled"),
        BlockingError::Error(e) => e,
    })
}

fn write_file_sync_unencrypted(path: PathBuf, contents: &[u8]) -> Result<(), anyhow::Error> {
    log::trace!("Writing unencrypted file {:?}", path);

    use std::io::Write;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(contents)?;

    Ok(())
}

fn write_file_sync_encrypted(
    keys: [u8; 16 + 20],
    path: PathBuf,
    contents: &[u8],
) -> Result<(), anyhow::Error> {
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
        let cipher = Cbc::<Aes128, Pkcs7>::new_from_slices(&keys[0..16], &iv)
            .context("CBC initialization error")?;
        cipher.encrypt_vec(contents)
    };

    let mac = {
        use hmac::{Hmac, Mac, NewMac};
        use sha2::Sha256;
        // Verify HMAC SHA256, 32 last bytes
        let mut mac = Hmac::<Sha256>::new_from_slice(&keys[16..])
            .map_err(|_| anyhow::anyhow!("MAC keylength error"))?;
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
) -> Result<(), anyhow::Error> {
    match keys {
        Some(keys) => write_file_sync_encrypted(keys, path, contents),
        None => write_file_sync_unencrypted(path, contents),
    }
}

async fn write_file(
    keys: Option<[u8; 16 + 20]>,
    path: PathBuf,
    contents: Vec<u8>,
) -> Result<(), anyhow::Error> {
    actix_threadpool::run(move || write_file_sync(keys, path, &contents)).await?;
    Ok(())
}

fn load_file_sync_unencrypted(path: PathBuf) -> Result<Vec<u8>, anyhow::Error> {
    log::trace!("Opening unencrypted file {:?}", path);
    let contents = std::fs::read(&path)?;
    let count = contents.len();
    log::trace!("Read {:?}, {} bytes", path, count);
    Ok(contents)
}

fn load_file_sync_encrypted(keys: [u8; 16 + 20], path: PathBuf) -> Result<Vec<u8>, anyhow::Error> {
    // XXX This is *full* of bad practices.
    // Let's try to migrate to nacl or something alike in the future.

    log::trace!("Opening encrypted file {:?}", path);
    let mut contents = std::fs::read(&path)?;
    let count = contents.len();

    log::trace!("Read {:?}, {} bytes", path, count);
    anyhow::ensure!(count >= 16 + 32, "File smaller than cryptographic overhead");

    let (iv, contents) = contents.split_at_mut(16);
    let count = count - 16;
    let (contents, mac) = contents.split_at_mut(count - 32);

    {
        use hmac::{Hmac, Mac, NewMac};
        use sha2::Sha256;
        // Verify HMAC SHA256, 32 last bytes
        let mut verifier = Hmac::<Sha256>::new_from_slice(&keys[16..])
            .map_err(|_| anyhow::anyhow!("MAC keylength error"))?;
        verifier.update(iv);
        verifier.update(contents);
        verifier
            .verify(mac)
            .map_err(|_| anyhow::anyhow!("MAC error"))?;
    }

    use aes::Aes128;
    use block_modes::block_padding::Pkcs7;
    use block_modes::{BlockMode, Cbc};
    // Decrypt password
    let cipher = Cbc::<Aes128, Pkcs7>::new_from_slices(&keys[0..16], iv)
        .context("CBC initialization error")?;
    Ok(cipher
        .decrypt(contents)
        .context("AES CBC decryption error")?
        .to_owned())
}

fn load_file_sync(keys: Option<[u8; 16 + 20]>, path: PathBuf) -> Result<Vec<u8>, anyhow::Error> {
    match keys {
        Some(keys) => load_file_sync_encrypted(keys, path),
        None => load_file_sync_unencrypted(path),
    }
}

async fn load_file(keys: Option<[u8; 16 + 20]>, path: PathBuf) -> Result<Vec<u8>, anyhow::Error> {
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
                .left_join(schema::group_v1s::table)
                .left_join(schema::group_v2s::table);
            $b
        };
        let triple: Option<(
            orm::DbSession,
            Option<orm::Recipient>,
            Option<orm::GroupV1>,
            Option<orm::GroupV2>,
        )> = query.first(&*db).ok();
        triple.map(Into::into)
    }};
}
macro_rules! fetch_sessions {
    ($db:expr, |$fragment:ident| $b:block ) => {{
        let db = $db;
        let query = {
            let $fragment = schema::sessions::table
                .left_join(schema::recipients::table)
                .left_join(schema::group_v1s::table)
                .left_join(schema::group_v2s::table);
            $b
        };
        let triples: Vec<(
            orm::DbSession,
            Option<orm::Recipient>,
            Option<orm::GroupV1>,
            Option<orm::GroupV2>,
        )> = query.load(&*db).unwrap();
        triples.into_iter().map(orm::Session::from).collect()
    }};
}

impl Storage {
    /// Returns the path to the storage.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn scaffold_directories(root: impl AsRef<Path>) -> Result<(), anyhow::Error> {
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
                    anyhow::bail!(
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
    ) -> Result<Storage, anyhow::Error> {
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
        let db = Self::open_db(db_path, path, password).await?;

        // 3. initialize protocol store
        let keys = match password {
            None => None,
            Some(pass) => Some(derive_storage_key(pass.to_string(), storage_salt_path).await?),
        };

        let identity_key_pair = protocol::IdentityKeyPair::generate(&mut rand::thread_rng());

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
            protocol_store: Arc::new(tokio::sync::RwLock::new(protocol_store)),
            credential_cache: Arc::new(Mutex::new(InMemoryCredentialsCache::default())),
            path: path.to_path_buf(),
        })
    }

    pub async fn open<T: AsRef<Path>>(
        db_path: &StorageLocation<T>,
        password: Option<String>,
    ) -> Result<Storage, anyhow::Error> {
        let path: &Path = std::ops::Deref::deref(db_path);

        let db = Self::open_db(db_path, path, password.as_deref()).await?;

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
            protocol_store: Arc::new(tokio::sync::RwLock::new(protocol_store)),
            credential_cache: Arc::new(Mutex::new(InMemoryCredentialsCache::default())),
            path: path.to_path_buf(),
        })
    }

    async fn open_db<T: AsRef<Path>>(
        db_path: &StorageLocation<T>,
        path: &Path,
        password: Option<&str>,
    ) -> Result<SqliteConnection, anyhow::Error> {
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

        // Run migrations.
        // We execute the transactions without foreign key checking enabled.
        // This is because foreign_keys=OFF implies that foreign key references are
        // not renamed when their parent table is renamed on *old SQLite version*.
        // https://stackoverflow.com/questions/67006159/how-to-re-parent-a-table-foreign-key-in-sqlite-after-recreating-the-parent
        // We can very probably do normal foreign_key checking again when we are on a more recent
        // SQLite.
        // That said, our check_foreign_keys() does output more useful information for when things
        // go haywire, albeit a bit later.
        db.execute("PRAGMA foreign_keys = OFF;").unwrap();
        db.transaction(|| -> Result<(), anyhow::Error> {
            embedded_migrations::run(&db)?;
            crate::check_foreign_keys(&db)?;
            Ok(())
        })?;
        db.execute("PRAGMA foreign_keys = ON;").unwrap();

        Ok(db)
    }

    /// Asynchronously loads the signal HTTP password from storage and decrypts it.
    pub async fn signal_password(&self) -> Result<String, anyhow::Error> {
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
    pub async fn signaling_key(&self) -> Result<[u8; 52], anyhow::Error> {
        let v = self
            .load_file(
                self.path
                    .join("storage")
                    .join("identity")
                    .join("http_signaling_key"),
            )
            .await?;
        anyhow::ensure!(v.len() == 52, "Signaling key is 52 bytes");
        let mut out = [0u8; 52];
        out.copy_from_slice(&v);
        Ok(out)
    }

    async fn load_file(&self, path: PathBuf) -> Result<Vec<u8>, anyhow::Error> {
        load_file(self.keys, path).await
    }

    /// Process message and store in database and update or create a session.
    ///
    /// This assumes that the metadata (source_e164 and source_uuid) are correct and verified.
    pub fn process_message(
        &self,
        mut new_message: NewMessage,
        session: Option<orm::Session>,
    ) -> (orm::Message, orm::Session) {
        let session = session.unwrap_or_else(|| {
            let recipient = self.merge_and_fetch_recipient(
                new_message.source_e164.as_deref(),
                new_message.source_uuid.as_deref(),
                TrustLevel::Certain,
            );
            self.fetch_or_insert_session_by_recipient_id(recipient.id)
        });

        new_message.session_id = Some(session.id);
        let message = self.create_message(&new_message);
        (message, session)
    }

    pub fn fetch_self_recipient(&self, cfg: &SignalConfig) -> Option<orm::Recipient> {
        let e164 = cfg.get_tel_clone();
        let uuid = cfg.get_uuid_clone();
        let e164 = if e164.is_empty() {
            log::warn!("No e164 set, cannot fetch self.");
            return None;
        } else {
            Some(e164.as_str())
        };
        let uuid = if uuid.is_empty() {
            log::warn!("No uuid set. Continuing with only e164");
            None
        } else {
            Some(uuid.as_str())
        };
        Some(self.merge_and_fetch_recipient(e164, uuid, TrustLevel::Certain))
    }

    pub fn fetch_recipient_by_e164(&self, new_e164: &str) -> Option<orm::Recipient> {
        use crate::schema::recipients::dsl::*;

        let db = self.db.lock();

        recipients.filter(e164.eq(new_e164)).first(&*db).ok()
    }

    pub fn compress_db(&self) -> usize {
        let db = self.db.lock();
        db.execute("VACUUM;").unwrap()
    }

    pub fn fetch_recipients(&self) -> Vec<orm::Recipient> {
        let db = self.db.lock();
        schema::recipients::table.load(&*db).expect("db")
    }

    pub fn fetch_recipient(
        &self,
        e164: Option<&str>,
        uuid: Option<&str>,
    ) -> Option<orm::Recipient> {
        if e164.is_none() && uuid.is_none() {
            panic!("fetch_recipient requires at least one of e164 or uuid");
        }

        let db = self.db.lock();
        use schema::recipients;
        let by_e164: Option<orm::Recipient> = e164
            .as_deref()
            .map(|e164| {
                recipients::table
                    .filter(recipients::e164.eq(e164))
                    .first(&*db)
                    .optional()
            })
            .transpose()
            .expect("db")
            .flatten();
        let by_uuid: Option<orm::Recipient> = uuid
            .as_deref()
            .map(|uuid| {
                recipients::table
                    .filter(recipients::uuid.eq(uuid))
                    .first(&*db)
                    .optional()
            })
            .transpose()
            .expect("db")
            .flatten();
        by_uuid.or(by_e164)
    }

    pub fn update_profile_key(
        &self,
        e164: Option<&str>,
        uuid: Option<&str>,
        new_profile_key: &[u8],
        trust_level: TrustLevel,
    ) -> orm::Recipient {
        // XXX check profile_key length
        let recipient = self.merge_and_fetch_recipient(e164, uuid, trust_level);

        let is_unset = recipient.profile_key.is_none()
            || recipient.profile_key.as_ref().map(Vec::len) == Some(0);

        if is_unset || trust_level == TrustLevel::Certain {
            if recipient.profile_key.as_deref() == Some(new_profile_key) {
                log::trace!("Profile key up-to-date");
                return recipient;
            }

            use crate::schema::recipients::dsl::*;
            let db = self.db.lock();
            diesel::update(recipients)
                .set(profile_key.eq(new_profile_key))
                .filter(id.eq(recipient.id))
                .execute(&*db)
                .expect("existing record updated");
            log::info!("Updated profile key for {}", recipient.e164_or_uuid());
        }
        // Re-fetch recipient with updated key
        self.fetch_recipient_by_id(recipient.id)
            .expect("fetch existing record")
    }

    /// Equivalent of Androids `RecipientDatabase::getAndPossiblyMerge`.
    pub fn merge_and_fetch_recipient(
        &self,
        e164: Option<&str>,
        uuid: Option<&str>,
        trust_level: TrustLevel,
    ) -> orm::Recipient {
        let db = self.db.lock();
        db.transaction(|| -> Result<orm::Recipient, diesel::result::Error> {
            self.merge_and_fetch_recipient_inner(e164, uuid, trust_level)
        })
        .expect("database")
    }

    // Inner method because the coverage report is then sensible.
    fn merge_and_fetch_recipient_inner(
        &self,
        e164: Option<&str>,
        uuid: Option<&str>,
        trust_level: TrustLevel,
    ) -> Result<orm::Recipient, diesel::result::Error> {
        if e164.is_none() && uuid.is_none() {
            panic!("merge_and_fetch_recipient requires at least one of e164 or uuid");
        }

        let db = self.db.lock();

        use schema::recipients;
        let by_e164: Option<orm::Recipient> = e164
            .as_deref()
            .map(|e164| {
                recipients::table
                    .filter(recipients::e164.eq(e164))
                    .first(&*db)
                    .optional()
            })
            .transpose()?
            .flatten();
        let by_uuid: Option<orm::Recipient> = uuid
            .as_deref()
            .map(|uuid| {
                recipients::table
                    .filter(recipients::uuid.eq(uuid))
                    .first(&*db)
                    .optional()
            })
            .transpose()?
            .flatten();

        match (by_e164, by_uuid) {
            (Some(by_e164), Some(by_uuid)) if by_e164.id == by_uuid.id => {
                // Both are equal, easy.
                Ok(by_uuid)
            }
            (Some(by_e164), Some(by_uuid)) => {
                log::warn!(
                    "Conflicting results for {} and {}. Finding a resolution.",
                    by_e164.e164.as_ref().unwrap(),
                    by_uuid.uuid.as_ref().unwrap()
                );
                match (by_e164.uuid, trust_level) {
                    (Some(_uuid), TrustLevel::Certain) => {
                        log::info!("Differing UUIDs, high trust, likely case of reregistration. Stripping the old account, updating new.");
                        // Strip the old one
                        diesel::update(recipients::table)
                            .set(recipients::e164.eq::<Option<String>>(None))
                            .filter(recipients::id.eq(by_e164.id))
                            .execute(&*db)?;
                        // Set the new one
                        diesel::update(recipients::table)
                            .set(recipients::e164.eq(e164))
                            .filter(recipients::id.eq(by_uuid.id))
                            .execute(&*db)?;
                        // Fetch again for the update
                        Ok(self
                            .fetch_recipient_by_id(by_uuid.id)
                            .expect("existing updated recipient"))
                    }
                    (Some(_uuid), TrustLevel::Uncertain) => {
                        log::info!("Differing UUIDs, low trust, likely case of reregistration. Doing absolutely nothing. Sorry.");
                        Ok(by_uuid)
                    }
                    (None, TrustLevel::Certain) => {
                        log::info!(
                            "Merging contacts: one with e164, the other only uuid, high trust."
                        );
                        let merged = self.merge_recipients(by_e164.id, by_uuid.id);
                        // XXX probably more recipient identifiers should be moved
                        diesel::update(recipients::table)
                            .set(recipients::e164.eq(e164))
                            .filter(recipients::id.eq(merged.id))
                            .execute(&*db)?;

                        Ok(self
                            .fetch_recipient_by_id(merged.id)
                            .expect("updated recipient"))
                    }
                    (None, TrustLevel::Uncertain) => {
                        log::info!(
                            "Not merging contacts: one with e164, the other only uuid, low trust."
                        );
                        Ok(by_uuid)
                    }
                }
            }
            (None, Some(by_uuid)) => {
                if let Some(e164) = e164 {
                    match trust_level {
                        TrustLevel::Certain => {
                            log::info!(
                                "Found phone number {} for contact {}. High trust, so updating.",
                                e164,
                                by_uuid.uuid.as_ref().unwrap()
                            );
                            diesel::update(recipients::table)
                                .set(recipients::e164.eq(e164))
                                .filter(recipients::id.eq(by_uuid.id))
                                .execute(&*db)?;
                            Ok(self
                                .fetch_recipient_by_id(by_uuid.id)
                                .expect("existing updated recipient"))
                        }
                        TrustLevel::Uncertain => {
                            log::info!("Found phone number {} for contact {}. Low trust, so doing nothing. Sorry again.", e164, by_uuid.uuid.as_ref().unwrap());
                            Ok(by_uuid)
                        }
                    }
                } else {
                    Ok(by_uuid)
                }
            }
            (Some(by_e164), None) => {
                if let Some(uuid) = uuid {
                    match trust_level {
                        TrustLevel::Certain => {
                            log::info!(
                                "Found UUID {} for contact {}. High trust, so updating.",
                                uuid,
                                by_e164.e164.unwrap()
                            );
                            diesel::update(recipients::table)
                                .set(recipients::uuid.eq(uuid))
                                .filter(recipients::id.eq(by_e164.id))
                                .execute(&*db)?;
                            Ok(self
                                .fetch_recipient_by_id(by_e164.id)
                                .expect("existing updated recipient"))
                        }
                        TrustLevel::Uncertain => {
                            log::info!(
                                "Found UUID {} for contact {}. Low trust, creating a new contact.",
                                uuid,
                                by_e164.e164.unwrap()
                            );
                            Ok(self.fetch_or_insert_recipient_by_uuid(uuid))
                        }
                    }
                } else {
                    Ok(by_e164)
                }
            }
            (None, None) => {
                let insert_e164 = (trust_level == TrustLevel::Certain) || uuid.is_none();
                diesel::insert_into(recipients::table)
                    .values((
                        recipients::e164.eq(if insert_e164 { e164 } else { None }),
                        recipients::uuid.eq(uuid),
                    ))
                    .execute(&*db)
                    .expect("insert new recipient");

                Ok(self.fetch_latest_recipient().expect("inserted recipient"))
            }
        }
    }

    /// Merge source_id into dest_id.
    ///
    /// Executes `merge_recipient_inner` inside a transaction, and then returns the result.
    fn merge_recipients(&self, source_id: i32, dest_id: i32) -> orm::Recipient {
        let db = self.db.lock();
        db.transaction(|| -> Result<(), diesel::result::Error> {
            // Defer constraints, we're moving a lot of data, inside of a transaction,
            // and if we have a bug it definitely needs more research anyway.
            db.execute("PRAGMA defer_foreign_keys = ON;")?;
            self.merge_recipient_inner(source_id, dest_id)
        })
        .expect("consistent migration");

        log::trace!("Contact merge comitted.");

        self.fetch_recipient_by_id(dest_id)
            .expect("existing contact")
    }

    // Inner method because the coverage report is then sensible.
    fn merge_recipient_inner(
        &self,
        source_id: i32,
        dest_id: i32,
    ) -> Result<(), diesel::result::Error> {
        log::info!(
            "Merge of contacts {} and {}. Will move all into {}",
            source_id,
            dest_id,
            dest_id
        );

        use schema::*;

        let db = self.db.lock();

        // 1. Merge messages senders.
        let message_count = diesel::update(messages::table)
            .filter(messages::sender_recipient_id.eq(source_id))
            .set(messages::sender_recipient_id.eq(dest_id))
            .execute(&*db)?;
        log::trace!("Merging messages: {}", message_count);

        // 2. Merge group V1 membership:
        //    - Delete duplicate memberships.
        //      We fetch the dest_id group memberships,
        //      and delete the source_id memberships that have the same group.
        //      Ideally, this would be a single self-join query,
        //      but Diesel doesn't like that yet.
        let target_memberships_v1: Vec<String> = group_v1_members::table
            .select(group_v1_members::group_v1_id)
            .filter(group_v1_members::recipient_id.eq(dest_id))
            .load(&*db)?;
        let deleted_memberships_v1 = diesel::delete(group_v1_members::table)
            .filter(
                group_v1_members::group_v1_id
                    .eq_any(&target_memberships_v1)
                    .and(group_v1_members::recipient_id.eq(source_id)),
            )
            .execute(&*db)?;
        //    - Update the rest
        let updated_memberships_v1 = diesel::update(group_v1_members::table)
            .filter(group_v1_members::recipient_id.eq(source_id))
            .set(group_v1_members::recipient_id.eq(dest_id))
            .execute(&*db)?;
        log::trace!(
            "Merging Group V1 memberships: deleted duplicate {}/{}, moved {}/{}.",
            deleted_memberships_v1,
            target_memberships_v1.len(),
            updated_memberships_v1,
            target_memberships_v1.len()
        );

        // 3. Merge sessions:
        let source_session: Option<orm::DbSession> = sessions::table
            .filter(sessions::direct_message_recipient_id.eq(source_id))
            .first(&*db)
            .optional()?;
        let target_session: Option<orm::DbSession> = sessions::table
            .filter(sessions::direct_message_recipient_id.eq(dest_id))
            .first(&*db)
            .optional()?;
        match (source_session, target_session) {
            (Some(source_session), Some(target_session)) => {
                // Both recipients have a session.
                // Move the source session's messages to the target session,
                // then drop the source session.
                let updated_message_count = diesel::update(messages::table)
                    .filter(messages::session_id.eq(source_session.id))
                    .set(messages::session_id.eq(target_session.id))
                    .execute(&*db)?;
                let dropped_session_count = diesel::delete(sessions::table)
                    .filter(sessions::id.eq(source_session.id))
                    .execute(&*db)?;

                assert_eq!(dropped_session_count, 1, "Drop the single source session.");

                log::trace!(
                    "Updating source session's messages ({} total). Dropped source session.",
                    updated_message_count
                );
            }
            (Some(source_session), None) => {
                log::info!("Strange, no session for the target_id. Updating source.");
                let updated_session = diesel::update(sessions::table)
                    .filter(sessions::id.eq(source_session.id))
                    .set(sessions::direct_message_recipient_id.eq(dest_id))
                    .execute(&*db)?;
                assert_eq!(updated_session, 1, "Update source session");
            }
            (None, Some(_target_session)) => {
                log::info!("Strange, no session for the source_id. Continuing.");
            }
            (None, None) => {
                log::warn!("Strange, neither recipient has a session. Continuing.");
            }
        }

        // 4. Merge reactions
        //    This too would benefit from a subquery or self-join.
        let target_reactions: Vec<i32> = reactions::table
            .select(reactions::reaction_id)
            .filter(reactions::author.eq(dest_id))
            .load(&*db)?;
        // Delete duplicates from source.
        // We're not going to merge based on receive time,
        // although that would be the "right" thing to do.
        // Let's hope we never really take this path.
        let deleted_reactions = diesel::delete(reactions::table)
            .filter(
                reactions::author
                    .eq(source_id)
                    .and(reactions::message_id.eq_any(target_reactions)),
            )
            .execute(&*db)?;
        log::log!(
            if deleted_reactions > 0 {
                log::Level::Warn
            } else {
                log::Level::Trace
            },
            "Deleted {} reactions. Please file an issue if > 0",
            deleted_reactions
        );
        let updated_reactions = diesel::update(reactions::table)
            .filter(reactions::author.eq(source_id))
            .set(reactions::author.eq(dest_id))
            .execute(&*db)?;
        log::trace!("Updated {} reactions", updated_reactions);

        // 5. Update receipts
        //    Same thing: delete the duplicates (although merging would be better),
        //    and update the rest.
        let target_receipts: Vec<i32> = receipts::table
            .select(receipts::message_id)
            .filter(receipts::recipient_id.eq(dest_id))
            .load(&*db)?;
        let deleted_receipts = diesel::delete(receipts::table)
            .filter(
                receipts::recipient_id
                    .eq(source_id)
                    .and(receipts::message_id.eq_any(target_receipts)),
            )
            .execute(&*db)?;
        log::log!(
            if deleted_receipts > 0 {
                log::Level::Warn
            } else {
                log::Level::Trace
            },
            "Deleted {} receipts. Please file an issue if > 0",
            deleted_receipts
        );
        let updated_receipts = diesel::update(receipts::table)
            .filter(receipts::recipient_id.eq(source_id))
            .set(receipts::recipient_id.eq(dest_id))
            .execute(&*db)?;
        log::trace!("Updated {} receipts", updated_receipts);

        let deleted = diesel::delete(recipients::table)
            .filter(recipients::id.eq(source_id))
            .execute(&*db)?;
        log::trace!("Deleted {} recipient", deleted);
        assert_eq!(deleted, 1, "delete only one recipient");
        Ok(())
    }

    pub fn fetch_or_insert_recipient_by_uuid(&self, new_uuid: &str) -> orm::Recipient {
        use crate::schema::recipients::dsl::*;

        let db = self.db.lock();

        if let Ok(recipient) = recipients.filter(uuid.eq(new_uuid)).first(&*db) {
            recipient
        } else {
            diesel::insert_into(recipients)
                .values(uuid.eq(new_uuid))
                .execute(&*db)
                .expect("insert new recipient");
            recipients
                .filter(uuid.eq(new_uuid))
                .first(&*db)
                .expect("newly inserted recipient")
        }
    }

    pub fn fetch_or_insert_recipient_by_e164(&self, new_e164: &str) -> orm::Recipient {
        use crate::schema::recipients::dsl::*;

        let db = self.db.lock();

        if let Ok(recipient) = recipients.filter(e164.eq(new_e164)).first(&*db) {
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
    /// Should only be called if the relation between the e164 and uuid is certain.
    pub fn mark_message_received(
        &self,
        receiver_e164: Option<&str>,
        receiver_uuid: Option<&str>,
        timestamp: NaiveDateTime,
        delivered_at: Option<chrono::DateTime<Utc>>,
    ) -> Option<(orm::Session, orm::Message)> {
        let db = self.db.lock();

        // XXX: probably, the trigger for this method call knows a better time stamp.
        let delivered_at = delivered_at.unwrap_or_else(chrono::Utc::now).naive_utc();

        // Find the recipient
        let recipient =
            self.merge_and_fetch_recipient(receiver_e164, receiver_uuid, TrustLevel::Certain);
        let message_id = schema::messages::table
            .select(schema::messages::id)
            .filter(schema::messages::server_timestamp.eq(timestamp))
            .first(&*db)
            .optional()
            .expect("db");
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
                schema::receipts::recipient_id.eq(recipient.id),
                schema::receipts::delivered.eq(delivered_at),
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
            .filter(schema::receipts::recipient_id.eq(recipient.id))
            .set((schema::receipts::delivered.eq(delivered_at),))
            .execute(&*db);
        if let Err(e) = update {
            log::error!("Could not update receipt: {}", e);
        }

        let message = self.fetch_message_by_id(message_id)?;
        let session = self.fetch_session_by_id(message.session_id)?;

        Some((session, message))
    }

    /// Fetches the latest session by last_insert_rowid.
    ///
    /// This only yields correct results when the last insertion was in fact a session.
    fn fetch_latest_recipient(&self) -> Option<orm::Recipient> {
        use schema::recipients::dsl::*;
        let db = self.db.lock();
        recipients.filter(id.eq(last_insert_rowid)).first(&*db).ok()
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
        log::trace!("Called fetch_session_by_e164({})", e164);
        let db = self.db.lock();
        fetch_session!(db, |query| {
            query.filter(schema::recipients::e164.eq(e164))
        })
    }

    pub fn fetch_session_by_recipient_id(&self, rid: i32) -> Option<orm::Session> {
        log::trace!("Called fetch__session_by_recipient_id({})", rid);
        let db = self.db.lock();
        fetch_session!(db, |query| { query.filter(schema::recipients::id.eq(rid)) })
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

    pub fn group_v2_exists(&self, group: &GroupV2) -> bool {
        let group_id = group.secret.get_group_identifier();
        let group_id_hex = hex::encode(group_id);

        let db = self.db.lock();
        let group: Option<orm::GroupV2> = schema::group_v2s::table
            .filter(schema::group_v2s::id.eq(group_id_hex))
            .first(&*db)
            .optional()
            .expect("db");
        group.is_some()
    }

    pub fn fetch_group_members_by_group_v2_id(
        &self,
        id: &str,
    ) -> Vec<(orm::GroupV2Member, orm::Recipient)> {
        let db = self.db.lock();
        schema::group_v2_members::table
            .inner_join(schema::recipients::table)
            .filter(schema::group_v2_members::group_v2_id.eq(id))
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

    /// Fetches recipient's DM session, or creates the session.
    pub fn fetch_or_insert_session_by_recipient_id(&self, rid: i32) -> orm::Session {
        log::trace!("Called fetch_or_insert_session_by_recipient_id({})", rid);
        let db = self.db.lock();
        if let Some(session) = self.fetch_session_by_recipient_id(rid) {
            return session;
        }

        use schema::sessions::dsl::*;
        diesel::insert_into(sessions)
            .values((direct_message_recipient_id.eq(rid),))
            .execute(&*db)
            .unwrap();

        self.fetch_latest_session()
            .expect("a session has been inserted")
    }

    pub fn fetch_or_insert_session_by_group_v1(&self, group: &GroupV1) -> orm::Session {
        let db = self.db.lock();

        let group_id = hex::encode(&group.id);

        log::trace!(
            "Called fetch_or_insert_session_by_group_v1({}[..])",
            &group_id[..8]
        );

        if let Some(session) = fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::group_v1_id.eq(&group_id))
        }) {
            return session;
        }

        let new_group = orm::GroupV1 {
            id: group_id.clone(),
            name: group.name.clone(),
            expected_v2_id: None,
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

    pub fn fetch_session_by_group_v1_id(&self, group_id_hex: &str) -> Option<orm::Session> {
        if group_id_hex.len() != 32 {
            log::warn!(
                "Trying to fetch GV1 with ID of {} != 32 chars",
                group_id_hex.len()
            );
            return None;
        }
        fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::group_v1_id.eq(&group_id_hex))
        })
    }

    pub fn fetch_session_by_group_v2_id(&self, group_id_hex: &str) -> Option<orm::Session> {
        if group_id_hex.len() != 64 {
            log::warn!(
                "Trying to fetch GV2 with ID of {} != 64 chars",
                group_id_hex.len()
            );
            return None;
        }
        fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::group_v2_id.eq(&group_id_hex))
        })
    }

    pub fn fetch_or_insert_session_by_group_v2(&self, group: &GroupV2) -> orm::Session {
        let db = self.db.lock();

        let group_id = group.secret.get_group_identifier();
        let group_id_hex = hex::encode(group_id);

        log::trace!(
            "Called fetch_or_insert_session_by_group_v2({})",
            group_id_hex
        );

        if let Some(session) = fetch_session!(self.db.lock(), |query| {
            query.filter(schema::sessions::columns::group_v2_id.eq(&group_id_hex))
        }) {
            return session;
        }

        let master_key =
            bincode::serialize(&group.secret.get_master_key()).expect("serialized master key");
        let new_group = orm::GroupV2 {
            id: group_id_hex,
            // XXX qTr?
            name: "New V2 group (updating)".into(),
            master_key: hex::encode(master_key),
            revision: 0,

            invite_link_password: None,

            // We don't know the ACL levels.
            // 0 means UNKNOWN
            access_required_for_attributes: 0,
            access_required_for_members: 0,
            access_required_for_add_from_invite_link: 0,
        };

        // Group does not exist, insert first.
        diesel::insert_into(schema::group_v2s::table)
            .values(&new_group)
            .execute(&*db)
            .unwrap();

        // XXX somehow schedule this group for member list/name updating.

        // Two things could have happened by now:
        // - Migration: there is an existing session for a groupv1 with this V2 id.
        // - New group

        let migration_v1_session: Option<(orm::GroupV1, Option<orm::DbSession>)> =
            schema::group_v1s::table
                .filter(schema::group_v1s::expected_v2_id.eq(&new_group.id))
                .left_join(schema::sessions::table)
                .first(&*db)
                .optional()
                .expect("db");

        use schema::sessions::dsl::*;
        match migration_v1_session {
            Some((_group, Some(session))) => {
                log::info!(
                    "Group V2 migration detected. Updating session to point to the new group."
                );
                // XXX this probably needs to influence the GUI too...
                let count = diesel::update(sessions)
                    .set((
                        group_v1_id.eq::<Option<String>>(None),
                        group_v2_id.eq(&new_group.id),
                    ))
                    .filter(id.eq(session.id))
                    .execute(&*db)
                    .expect("session updated");
                // XXX consider removing the group_v1
                assert_eq!(count, 1, "session should have been updated");
                // Refetch session because the info therein is stale.
                self.fetch_session_by_id(session.id)
                    .expect("existing session")
            }
            Some((_group, None)) => {
                unreachable!("Former group V1 found.  We expect the branch above to have returned a session for it.");
            }
            None => {
                diesel::insert_into(sessions)
                    .values((group_v2_id.eq(&new_group.id),))
                    .execute(&*db)
                    .unwrap();

                self.fetch_latest_session()
                    .expect("a session has been inserted")
            }
        }
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
        // XXX Storing the message with its attachments should happen in a transaction.
        // Meh.
        let db = self.db.lock();

        let session = new_message.session_id.expect("session id");

        log::trace!("Called create_message(..) for session {}", session);

        let has_source = new_message.source_e164.is_some() || new_message.source_uuid.is_some();
        let sender_id = if has_source {
            self.fetch_recipient(
                new_message.source_e164.as_deref(),
                new_message.source_uuid.as_deref(),
            )
            .map(|r| r.id)
        } else {
            None
        };

        // The server time needs to be the rounded-down version;
        // chrono does nanoseconds.
        let server_time = millis_to_naive_chrono(new_message.timestamp.timestamp_millis() as u64);

        let affected_rows = {
            use schema::messages::dsl::*;
            diesel::insert_into(messages)
                .values((
                    session_id.eq(session),
                    text.eq(&new_message.text),
                    sender_recipient_id.eq(sender_id),
                    received_timestamp.eq(if !new_message.outgoing {
                        Some(chrono::Utc::now().naive_utc())
                    } else {
                        None
                    }),
                    sent_timestamp.eq(if new_message.outgoing && new_message.sent {
                        Some(new_message.timestamp)
                    } else {
                        None
                    }),
                    server_timestamp.eq(server_time),
                    is_read.eq(new_message.is_read),
                    is_outbound.eq(new_message.outgoing),
                    flags.eq(new_message.flags),
                ))
                .execute(&*db)
                .expect("inserting a message")
        };

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

        if let Some(path) = &new_message.attachment {
            let affected_rows = {
                use schema::attachments::dsl::*;
                diesel::insert_into(attachments)
                    .values((
                        message_id.eq(latest_message.id),
                        content_type.eq(new_message.mime_type.as_ref().unwrap()),
                        attachment_path.eq(path),
                        is_voice_note.eq(false),
                        is_borderless.eq(false),
                        is_quote.eq(false),
                    ))
                    .execute(&*db)
                    .expect("Insert attachment")
            };

            assert_eq!(
                affected_rows, 1,
                "Did not insert the attachment. Dazed and confused."
            );
        }

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
            .order_by(schema::messages::columns::server_timestamp.desc())
            .load(&*db)
            .expect("database")
    }

    /// Returns a vector of tuples of messages with their sender.
    ///
    /// When the sender is None, it is a sent message, not a received message.
    // XXX maybe this should be `Option<Vec<...>>`.
    pub fn fetch_all_messages_augmented(&self, sid: i32) -> Vec<orm::AugmentedMessage> {
        let db = self.db.lock();

        // XXX double/aliased-join would be very useful.
        // Our strategy is to fetch as much as possible, and to augment with as few additional
        // queries as possible. We chose to not join `sender`, and instead use a loop for that
        // part.
        log::trace!("Called fetch_all_messages_augmented({})", sid);
        let messages = self.fetch_all_messages(sid);

        let attachments: Vec<orm::Attachment> = schema::attachments::table
            .select(schema::attachments::all_columns)
            .inner_join(schema::messages::table.inner_join(schema::sessions::table))
            .filter(schema::sessions::id.eq(sid))
            .order_by(schema::attachments::message_id.desc())
            .load(&*db)
            .expect("db");

        let receipts: Vec<(orm::Receipt, orm::Recipient)> = schema::receipts::table
            .inner_join(schema::recipients::table)
            .select((
                schema::receipts::all_columns,
                schema::recipients::all_columns,
            ))
            .inner_join(schema::messages::table.inner_join(schema::sessions::table))
            .filter(schema::sessions::id.eq(sid))
            .order_by(schema::receipts::message_id.desc())
            .load(&*db)
            .expect("db");

        let attachments = attachments
            .into_iter()
            .group_by(|attachment| attachment.message_id);
        let mut attachments = attachments.into_iter().peekable();
        let receipts = receipts
            .into_iter()
            .group_by(|(receipt, _recipient)| receipt.message_id);
        let mut receipts = receipts.into_iter().peekable();

        let mut aug_messages = Vec::with_capacity(messages.len());
        for (message, sender) in messages {
            let attachments = if attachments
                .peek()
                .map(|(id, _)| *id == message.id)
                .unwrap_or(false)
            {
                let (_, attachments) = attachments.next().unwrap();
                attachments.collect_vec()
            } else {
                vec![]
            };
            let receipts = if receipts
                .peek()
                .map(|(id, _)| *id == message.id)
                .unwrap_or(false)
            {
                let (_, receipts) = receipts.next().unwrap();
                receipts.collect_vec()
            } else {
                vec![]
            };
            aug_messages.push(orm::AugmentedMessage {
                inner: message,
                sender,
                attachments,
                receipts,
            });
        }
        aug_messages
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
    pub async fn peer_identity(&self, e164: String) -> Result<String, anyhow::Error> {
        let addr = ProtocolAddress::new(e164, 1);
        let ident = self
            .get_identity(&addr, None)
            .await?
            .context("No such identity")?;
        Ok(hex::encode_upper(ident.serialize()))
    }

    pub fn credential_cache(&self) -> std::sync::MutexGuard<InMemoryCredentialsCache> {
        self.credential_cache.lock().expect("mutex")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest(ext, case("mp4"), case("jpg"), case("jpg"), case("png"), case("txt"))]
    #[actix_rt::test]
    async fn test_save_attachment(ext: &str) {
        use std::env;
        use std::fs;
        use std::path::Path;

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
    fn encrypt_and_decrypt_file() -> Result<(), anyhow::Error> {
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

    #[rstest(
        storage_password,
        case(Some(String::from("some password"))),
        case(None)
    )]
    #[actix_rt::test]
    async fn test_create_and_open_storage(
        storage_password: Option<String>,
    ) -> Result<(), anyhow::Error> {
        use rand::distributions::Alphanumeric;
        use rand::{Rng, RngCore};

        env_logger::try_init().ok();

        let location = super::temp();
        let rng = rand::thread_rng();

        // Signaling password for REST API
        let password: String = rng.sample_iter(&Alphanumeric).take(24).collect();

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
                use libsignal_service::prelude::protocol::IdentityKeyStore;
                // TODO: assert that tables exist
                assert_eq!(password, $storage.signal_password().await?);
                assert_eq!(signaling_key, $storage.signaling_key().await?);
                assert_eq!(regid, $storage.get_local_registration_id(None).await?);

                let (signed, unsigned) = $storage.next_pre_key_ids().await;
                // Unstarted client will have no pre-keys.
                assert_eq!(0, signed);
                assert_eq!(0, unsigned);

                Result::<_, anyhow::Error>::Ok(())
            }};
        }

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
