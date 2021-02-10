use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::commons::api::Token;
use crate::commons::error::Error;
use crate::commons::KrillResult;

use super::crypt;

const TAG_SIZE: usize = 16;
const MAX_CACHE_SECS: u64 = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientSession {
    pub start_time: u64,
    pub expires_in: Option<Duration>,
    pub id: String,
    pub attributes: HashMap<String, String>,
    pub secrets: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum SessionStatus {
    Active,
    NeedsRefresh,
    Expired,
}

impl ClientSession {
    pub fn status(&self) -> SessionStatus {
        if let Some(expires_in) = &self.expires_in {
            match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(now) => {
                    let cur_age_secs = now.as_secs() - self.start_time;
                    let max_age_secs = expires_in.as_secs();

                    let status = if cur_age_secs > max_age_secs {
                        SessionStatus::Expired
                    } else if cur_age_secs > (max_age_secs.checked_div(2).unwrap()) {
                        SessionStatus::NeedsRefresh
                    } else {
                        SessionStatus::Active
                    };

                    trace!(
                        "Login session status check: id={}, status={:?}, max age={} secs, cur age={} secs",
                        &self.id,
                        &status,
                        max_age_secs,
                        cur_age_secs
                    );

                    return status;
                }
                Err(err) => {
                    warn!(
                        "Login session status check: unable to determine the current time: {}",
                        err
                    );
                }
            }
        }

        SessionStatus::Active
    }
}

struct CachedSession {
    pub evict_after: u64,
    pub session: ClientSession,
}

/// A short term cache to reduce the impact of session token decryption and
/// deserialization (e.g. for multiple requests in a short space of time by the
/// Lagosta UI client) while keeping potentially sensitive data in-memory for as
/// short as possible. This cache is NOT responsible for enforcing token
/// expiration, that is handled separately by the AuthProvider.
pub struct LoginSessionCache {
    cache: RwLock<HashMap<Token, CachedSession>>,
    encrypt_fn: fn(&[u8], &[u8], &mut [u8]) -> KrillResult<Vec<u8>>,
    decrypt_fn: fn(&[u8], &[u8], &[u8]) -> KrillResult<Vec<u8>>,
    ttl_secs: u64,
}

impl Default for LoginSessionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginSessionCache {
    pub fn new() -> Self {
        LoginSessionCache {
            cache: RwLock::new(HashMap::new()),
            encrypt_fn: crypt::encrypt,
            decrypt_fn: crypt::decrypt,
            ttl_secs: MAX_CACHE_SECS,
        }
    }

    pub fn with_ttl(self, ttl_secs: u64) -> Self {
        LoginSessionCache {
            cache: self.cache,
            encrypt_fn: self.encrypt_fn,
            decrypt_fn: self.decrypt_fn,
            ttl_secs,
        }
    }

    pub fn with_encrypter(self, encrypt_fn: fn(&[u8], &[u8], &mut [u8]) -> KrillResult<Vec<u8>>) -> Self {
        LoginSessionCache {
            cache: self.cache,
            encrypt_fn: encrypt_fn,
            decrypt_fn: self.decrypt_fn,
            ttl_secs: self.ttl_secs,
        }
    }

    pub fn with_decrypter(self, decrypt_fn: fn(&[u8], &[u8], &[u8]) -> KrillResult<Vec<u8>>) -> Self {
        LoginSessionCache {
            cache: self.cache,
            encrypt_fn: self.encrypt_fn,
            decrypt_fn: decrypt_fn,
            ttl_secs: self.ttl_secs,
        }
    }

    fn time_now_secs_since_epoch() -> KrillResult<u64> {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| Error::Custom(format!("Unable to determine the current time: {}", err)))?
            .as_secs())
    }

    fn lookup_session(&self, token: &Token) -> Option<ClientSession> {
        match self.cache.read() {
            Ok(readable_cache) => {
                if let Some(cache_item) = readable_cache.get(&token) {
                    return Some(cache_item.session.clone());
                }
            }
            Err(err) => warn!("Unexpected session cache miss: {}", err),
        }

        None
    }

    fn cache_session(&self, token: &Token, session: &ClientSession) {
        match self.cache.write() {
            Ok(mut writeable_cache) => match Self::time_now_secs_since_epoch() {
                Ok(now) => {
                    writeable_cache.insert(
                        token.clone(),
                        CachedSession {
                            evict_after: now + self.ttl_secs,
                            session: session.clone(),
                        },
                    );
                }
                Err(err) => warn!("Unable to cache decrypted session token: {}", err),
            },
            Err(err) => warn!("Unable to cache decrypted session token: {}", err),
        }
    }

    pub fn encode(
        &self,
        id: &str,
        attributes: &HashMap<String, String>,
        secrets: &[String],
        key: &[u8],
        expires_in: Option<Duration>,
    ) -> KrillResult<Token> {
        let session = ClientSession {
            start_time: Self::time_now_secs_since_epoch()?,
            expires_in,
            id: id.to_string(),
            attributes: attributes.clone(),
            secrets: secrets.to_vec(),
        };

        debug!("Creating token for session: {:?}", &session);

        let session_json_str = serde_json::to_string(&session)
            .map_err(|err| Error::Custom(format!("Error while serializing session data: {}", err)))?;
        let unencrypted_bytes = session_json_str.as_bytes();

        let mut tag: [u8; 16] = [0; 16];
        let mut encrypted_bytes = (self.encrypt_fn)(key, unencrypted_bytes, &mut tag)?;
        encrypted_bytes.extend(tag.iter());

        let token = Token::from(base64::encode(&encrypted_bytes));

        self.cache_session(&token, &session);
        Ok(token)
    }

    pub fn decode(&self, token: Token, key: &[u8]) -> KrillResult<ClientSession> {
        if let Some(session) = self.lookup_session(&token) {
            trace!("Session cache hit for session id {}", &session.id);
            return Ok(session);
        } else {
            trace!("Session cache miss, deserializing...");
        }

        let bytes = base64::decode(token.as_ref().as_bytes())
            .map_err(|err| Error::ApiInvalidCredentials(format!("Invalid bearer token: {}", err)))?;

        if bytes.len() <= TAG_SIZE {
            return Err(Error::ApiInvalidCredentials(
                "Invalid bearer token: token is too short".to_string(),
            ));
        }

        let encrypted_len = bytes.len() - TAG_SIZE;
        let (encrypted_bytes, tag_bytes) = bytes.split_at(encrypted_len);
        let unencrypted_bytes = (self.decrypt_fn)(key, encrypted_bytes, tag_bytes)?;

        let session = serde_json::from_slice::<ClientSession>(&unencrypted_bytes)
            .map_err(|err| Error::Custom(format!("Unable to deserializing client session: {}", err)))?;

        trace!("Session cache miss, deserialized session id {}", &session.id);

        self.cache_session(&token, &session);

        Ok(session)
    }

    pub fn remove(&self, token: &Token) {
        match self.cache.write() {
            Ok(mut writeable_cache) => {
                writeable_cache.remove(token);
            }
            Err(err) => warn!("Unable to purge cached session: {}", err),
        }
    }

    pub fn size(&self) -> usize {
        match self.cache.read() {
            Ok(readable_cache) => readable_cache.len(),
            Err(err) => {
                warn!("Unable to query session cache size: {}", err);
                0
            }
        }
    }

    pub fn sweep(&self) -> KrillResult<()> {
        let mut cache = self
            .cache
            .write()
            .map_err(|err| Error::Custom(format!("Unable to purge session cache: {}", err)))?;

        let size_before = cache.len();
        let now = Self::time_now_secs_since_epoch()?;

        // Only retain cache items that have been cached for less than the
        // maximum time allowed.
        cache.retain(|_, v| v.evict_after > now);

        let size_after = size_before - cache.len();

        debug!(
            "Login session cache purge: size before={}, size after={}",
            size_before, size_after
        );

        Ok(())
    }
}

mod tests {
    #[test]
    fn basic_login_session_cache_test() {
        use super::*;

        const KEY: &[u8] = "unused".as_bytes();

        // Create a new cache whose items are elligible for eviction after one
        // second and which does no actual encryption or decryption.
        let cache = LoginSessionCache::new()
            .with_ttl(1)
            .with_encrypter(|_, v, _| Ok(v.to_vec()))
            .with_decrypter(|_, v, _| Ok(v.to_vec()));

        // Add an item to the cache and verify that the cache now has 1 item
        let item1_token = cache.encode("some id", &HashMap::new(), &[], KEY, None).unwrap();
        assert_eq!(cache.size(), 1);

        let item1 = cache.decode(item1_token, KEY).unwrap();
        assert_eq!(item1.id, "some id");
        assert_eq!(item1.attributes, HashMap::new());
        assert_eq!(item1.expires_in, None);
        assert_eq!(item1.secrets, Vec::<String>::new());

        // Wait until after the cached item should have expired but as the cache
        // has not yet been swept the item should still be in the cache
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(cache.size(), 1);

        // Add another item to the cache
        let mut some_attrs = HashMap::new();
        some_attrs.insert(String::from("some attr key"), String::from("some attr val"));
        let item2_token = cache
            .encode(
                "other id",
                &some_attrs,
                &[String::from("some secret")],
                KEY,
                Some(Duration::from_secs(10)),
            )
            .unwrap();
        assert_eq!(cache.size(), 2);

        // Sweep the cache and confirm that the expired cache item has been
        // removed but the newest cache item remains.
        cache.sweep().unwrap();
        assert_eq!(cache.size(), 1);

        // Wait until after the remaining cached item should have expired but as
        // the cache has not yet been swept the item should still be present.
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(cache.size(), 1);

        let item2 = cache.decode(item2_token, KEY).unwrap();
        let mut again_some_attrs = HashMap::new();
        again_some_attrs.insert(String::from("some attr key"), String::from("some attr val"));
        assert_eq!(item2.id, "other id");
        assert_eq!(item2.attributes, again_some_attrs);
        assert_eq!(item2.expires_in, Some(Duration::from_secs(10)));
        assert_eq!(item2.secrets, [String::from("some secret")]);

        // Sweep the cache and confirm that cache is now empty.
        cache.sweep().unwrap();
        assert_eq!(cache.size(), 0);
    }
}
