use crate::error::Result;
use crate::metric;
use crate::models;
use crate::storage::Storage;
use crate::util;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use metrics::{histogram, increment_counter, register_histogram};
use redis::Commands;
use std::fmt;
use std::marker::Send;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::vec::Vec;

pub enum CacheData {
    TextData(String),
    BytesData(Bytes),
    ByteStream(
        Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin>,
        Option<usize>,
    ), // stream and size
}

impl CacheData {
    fn len(&self) -> usize {
        match &self {
            CacheData::TextData(text) => text.len(),
            CacheData::BytesData(bytes) => bytes.len(),
            CacheData::ByteStream(_, size) => size.unwrap(),
        }
    }
}

impl From<String> for CacheData {
    fn from(s: String) -> CacheData {
        CacheData::TextData(s)
    }
}

impl From<Bytes> for CacheData {
    fn from(bytes: Bytes) -> CacheData {
        CacheData::BytesData(bytes)
    }
}

impl From<Vec<u8>> for CacheData {
    fn from(vec: Vec<u8>) -> CacheData {
        CacheData::BytesData(Bytes::from(vec))
    }
}

impl AsRef<[u8]> for CacheData {
    fn as_ref(&self) -> &[u8] {
        // TODO:
        match &self {
            CacheData::TextData(text) => text.as_ref(),
            CacheData::BytesData(bytes) => bytes.as_ref(),
            _ => unimplemented!(),
        }
    }
}

impl fmt::Debug for CacheData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("CacheData");
        match &self {
            CacheData::TextData(s) => f.field("TextData", s),
            CacheData::BytesData(b) => f.field("Bytes", b),
            CacheData::ByteStream(_, size) => f.field(
                "ByteStream",
                &format!(
                    "(stream of size {})",
                    size.map(|x| format!("{}", x))
                        .unwrap_or("unknown".to_string())
                ),
            ),
        };
        f.finish()
    }
}

/// CachePolicy is a trait that defines the shared behaviors of all cache policies.
/// - `put`: put a key-value pair into the cache
/// - `get`: get a value from the cache
#[async_trait]
pub trait CachePolicy: Sync + Send {
    async fn put(&self, key: &str, entry: CacheData);
    async fn get(&self, key: &str) -> Option<CacheData>;
}

pub struct LruRedisCache {
    storage: Storage,
    pub size_limit: u64, // cache size in bytes(B)
    redis_client: redis::Client,
    id: String,
}

impl LruRedisCache {
    /// create a new LruRedisCache
    /// # Arguments
    /// * `root_dir`: the root directory of the cache in local fs
    /// * `size_limit`: the cache size limit in bytes
    /// * `redis_client`: a redis client to manage the cache metadata
    /// * `id`: the cache id, required to be unique among all `LruRedisCache` instances
    pub fn new(root_dir: &str, size_limit: u64, redis_client: redis::Client, id: &str) -> Self {
        debug!(
            "LRU Redis Cache init: id={} size_limit={}, root_dir={}",
            id, size_limit, root_dir
        );
        register_histogram!(Self::get_metric_key(id));
        Self {
            storage: Storage::FileSystem {
                root_dir: root_dir.to_string(),
            },
            size_limit,
            redis_client,
            id: id.to_string(),
        }
    }

    pub fn from_prefixed_key(&self, cache_key: &str) -> String {
        let cache_key = &cache_key[self.id.len() + 1..];
        cache_key.to_string()
    }

    fn get_total_size(&self) -> u64 {
        let key = self.total_size_key();
        let mut con = self.redis_client.get_connection().unwrap();
        let size = con.get::<&str, Option<u64>>(&key).unwrap().unwrap_or(0);
        histogram!(Self::get_metric_key(&self.id), size as f64);
        size
    }

    fn total_size_key(&self) -> String {
        self.to_prefixed_key("total_size")
    }

    /// returns the key to the zlist that stores the cache entries
    fn entries_zlist_key(&self) -> String {
        self.to_prefixed_key("cache_keys")
    }

    fn to_prefixed_key(&self, cache_key: &str) -> String {
        format!("{}_{}", self.id, cache_key)
    }

    fn get_metric_key(id: &str) -> String {
        format!("{}_{}", metric::HG_CACHE_SIZE_PREFIX, id)
    }
}

#[async_trait]
impl CachePolicy for LruRedisCache {
    /**
     * put a cache entry with given `key` as key and `entry` as value
     * An entry larger than the size limit of the current cache (self) is ignored.
     * If the size limit is exceeded after putting the entry, LRU eviction will run.
     * This function handles both local FS data and redis metadata.
     */
    async fn put(&self, key: &str, mut entry: CacheData) {
        let filename = key;
        let redis_key = &self.to_prefixed_key(key);
        // eviction policy
        let file_size = entry.len() as u64;
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();

        if file_size > self.size_limit {
            info!(
                "skip cache for {}, because its size exceeds the limit",
                redis_key
            );
        }
        // evict cache entry if necessary
        let _tx_result = redis::transaction(
            &mut sync_con,
            &[redis_key, &self.total_size_key(), &self.entries_zlist_key()],
            |con, _pipe| {
                let mut cur_cache_size = self.get_total_size();
                while cur_cache_size + file_size > self.size_limit {
                    // LRU eviction
                    trace!(
                        "current {} + new {} > limit {}",
                        con.get::<&str, Option<u64>>(&self.total_size_key())
                            .unwrap()
                            .unwrap_or(0),
                        file_size,
                        self.size_limit
                    );
                    let pkg_to_remove: Vec<(String, u64)> =
                        con.zpopmin(&self.entries_zlist_key(), 1).unwrap();
                    trace!("pkg_to_remove: {:?}", pkg_to_remove);
                    if pkg_to_remove.is_empty() {
                        info!("some files need to be evicted but they are missing from redis filelist. The cache metadata is inconsistent.");
                        return Err(redis::RedisError::from(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "cache metadata inconsistent",
                        )));
                    }
                    // remove from local fs and metadata in redis
                    for (f, _) in pkg_to_remove {
                        let file = self.from_prefixed_key(&f);
                        match self.storage.remove(&file) {
                            Ok(_) => {
                                increment_counter!(metric::CNT_RM_FILES);
                                info!("LRU cache removed {}", &file);
                            }
                            Err(e) => {
                                warn!("failed to remove file: {:?}", e);
                            }
                        };
                        let pkg_size: Option<u64> = con.hget(&f, "size").unwrap();
                        let _del_cnt = con.del::<&str, isize>(&f);
                        cur_cache_size = con
                            .decr::<&str, u64, u64>(&self.total_size_key(), pkg_size.unwrap_or(0))
                            .unwrap();
                        trace!("total_size -= {:?} -> {}", pkg_size, cur_cache_size);
                    }
                }
                Ok(Some(()))
            },
        );
        // cache to local filesystem
        self.storage.persist(filename, &mut entry).await;
        let entry = &CacheEntry::new(&redis_key, entry.len() as u64);
        let _redis_resp_str = models::set_lru_cache_entry(
            &mut sync_con,
            &redis_key,
            entry,
            &self.total_size_key(),
            &self.entries_zlist_key(),
        );
        trace!("CACHE SET {} -> {:?}", &redis_key, entry);
    }

    async fn get(&self, key: &str) -> Option<CacheData> {
        let filename = key;
        let redis_key = &self.to_prefixed_key(key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        let cache_result = models::get_cache_entry(&mut sync_con, redis_key).unwrap();
        if let Some(_cache_entry) = &cache_result {
            // cache hit
            // update cache entry in db
            let new_atime = util::now();
            match models::update_cache_entry_atime(
                &mut sync_con,
                redis_key,
                new_atime,
                &self.entries_zlist_key(),
            ) {
                Ok(_) => {}
                Err(e) => {
                    info!("Failed to update cache entry atime: {}", e);
                }
            }
            return match self.storage.read(filename).await {
                Ok(data) => {
                    trace!("CACHE GET [HIT] {} -> {:?} ", redis_key, &cache_result);
                    Some(data)
                }
                Err(_) => None,
            };
        };
        trace!("CACHE GET [MISS] {} -> {:?} ", redis_key, &cache_result);
        None
    }
}

/**
 *
 * TtlRedisCache is a simple cache policy that expire an existing cache entry
 * within the given TTL. The expiration is supported by redis.
 */
pub struct TtlRedisCache {
    storage: Storage,
    pub ttl: u64, // cache entry ttl in seconds
    redis_client: redis::Client,
    id: String,
    pub pending_close: Arc<AtomicBool>,
    pub expiration_thread_handler: Option<std::thread::JoinHandle<()>>,
}

impl TtlRedisCache {
    pub fn new(root_dir: &str, ttl: u64, redis_client: redis::Client, id: &str) -> Self {
        let cloned_client = redis_client.clone();
        let storage = Storage::FileSystem {
            root_dir: root_dir.to_string(),
        };
        let pending_close = Arc::new(AtomicBool::new(false));

        let id_clone = id.to_string();
        let storage_clone = storage.clone();
        let pending_close_clone = pending_close.clone();
        let expiration_thread_handler = std::thread::spawn(move || {
            debug!("TTL expiration listener is created!");
            loop {
                if pending_close_clone.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                match cloned_client.get_connection() {
                    Ok(mut con) => {
                        let mut pubsub = con.as_pubsub();
                        trace!("subscribe to cache key pattern: {}", &id_clone);
                        match pubsub.psubscribe(format!("__keyspace*__:{}*", &id_clone)) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to psubscribe: {}", e);
                                continue;
                            }
                        }
                        pubsub
                            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                            .unwrap();
                        loop {
                            // break if the associated cache object is about to be closed
                            if pending_close_clone.load(std::sync::atomic::Ordering::SeqCst) {
                                return;
                            }
                            match pubsub.get_message() {
                                Ok(msg) => {
                                    let channel: String = msg.get_channel().unwrap();
                                    let redis_key = &channel[channel.find(":").unwrap() + 1..];
                                    let file = Self::from_redis_key(&id_clone, &redis_key);
                                    trace!(
                                        "channel '{}': {}, file: {}",
                                        msg.get_channel_name(),
                                        channel,
                                        file,
                                    );
                                    match storage_clone.remove(&file) {
                                        Ok(_) => {
                                            increment_counter!(metric::CNT_RM_FILES);
                                            info!("TTL cache removed {}", &file);
                                        }
                                        Err(e) => {
                                            warn!("Failed to remove {}: {}", &file, e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    if e.kind() == redis::ErrorKind::IoError && e.is_timeout() {
                                        // ignore timeout error, as expected
                                    } else {
                                        error!(
                                            "Failed to get_message, retrying every 3s: {} {:?}",
                                            e,
                                            e.kind()
                                        );
                                        util::sleep_ms(3000);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to get redis connection: {}", e);
                        util::sleep_ms(3000);
                    }
                }
            }
        });
        Self {
            storage,
            ttl,
            redis_client,
            id: id.to_string(),
            pending_close,
            expiration_thread_handler: Some(expiration_thread_handler),
        }
    }

    pub fn to_redis_key(id: &str, cache_key: &str) -> String {
        format!("{}/{}", id, cache_key)
    }
    pub fn from_redis_key(id: &str, key: &str) -> String {
        String::from(&key[id.len() + 1..])
    }
}

#[async_trait]
impl CachePolicy for TtlRedisCache {
    async fn put(&self, key: &str, mut entry: CacheData) {
        let redis_key = Self::to_redis_key(&self.id, key);
        let filename = key;
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        self.storage.persist(filename, &mut entry).await;
        match models::set(&mut sync_con, &redis_key, "") {
            Ok(_) => {}
            Err(e) => {
                error!("set cache entry for {} failed: {}", key, e);
            }
        }
        match models::expire(&mut sync_con, &redis_key, self.ttl as usize) {
            Ok(_) => {}
            Err(e) => {
                error!("set cache entry ttl for {} failed: {}", key, e);
            }
        }
        trace!("CACHE SET {} TTL={}", &key, self.ttl);
    }

    async fn get(&self, key: &str) -> Option<CacheData> {
        let redis_key = Self::to_redis_key(&self.id, key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        match models::get(&mut sync_con, &redis_key) {
            Ok(res) => match res {
                Some(_) => match self.storage.read(key).await {
                    Ok(data) => {
                        trace!("GET {} [HIT]", key);
                        Some(data)
                    }
                    Err(_) => None,
                },
                None => None,
            },
            Err(e) => {
                info!("get cache entry key={} failed: {}", key, e);
                None
            }
        }
    }
}

impl Drop for TtlRedisCache {
    /// The spawned key expiration handler thread needs to be dropped.
    fn drop(&mut self) {
        self.pending_close
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread_handler) = self.expiration_thread_handler.take() {
            thread_handler.join().unwrap();
            trace!("spawned thread dropped.");
        } else {
            warn!("expiration_thread_handler is None! If the thread is not spawned in the first place, the cache may have not been working properly. Otherwise, a thread is leaked.");
        }
    }
}

#[derive(Hash, Eq, PartialEq, Debug)]
pub struct CacheEntry<Metadata, Key, Value> {
    pub metadata: Metadata,
    pub key: Key,
    pub value: Value,
}

#[derive(Debug)]
pub struct LruCacheMetadata {
    pub size: u64,
    pub atime: i64, // last access timestamp
}

impl CacheEntry<LruCacheMetadata, String, ()> {
    pub fn new(path: &str, size: u64) -> CacheEntry<LruCacheMetadata, String, ()> {
        CacheEntry {
            metadata: LruCacheMetadata {
                size: size,
                atime: util::now(),
            },
            key: String::from(path),
            value: (),
        }
    }

    /**
     * Convert a cache entry to an array keys and values to be stored as redis hash
     */
    pub fn to_redis_multiple_fields(&self) -> Vec<(&str, String)> {
        vec![
            ("path", self.key.clone()),
            ("size", self.metadata.size.to_string()),
            ("atime", self.metadata.atime.to_string()),
        ]
    }
}

pub struct NoCache {}

#[async_trait]
impl CachePolicy for NoCache {
    async fn put(&self, _key: &str, _entry: CacheData) {}
    async fn get(&self, _key: &str) -> Option<CacheData> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self};
    use futures::StreamExt;
    use std::fs;
    use std::io;
    use std::io::prelude::*;
    use std::thread;
    use std::time;

    impl CacheData {
        pub async fn to_vec(self) -> Vec<u8> {
            match self {
                CacheData::TextData(text) => text.into_bytes(),
                CacheData::BytesData(bytes) => bytes.to_vec(),
                CacheData::ByteStream(mut stream, ..) => {
                    let mut v = Vec::new();
                    while let Some(bytes_result) = stream.next().await {
                        if !bytes_result.is_ok() {
                            return Vec::new();
                        }
                        v.append(&mut bytes_result.unwrap().to_vec());
                    }
                    v
                }
            }
        }
    }

    static TEST_CACHE_DIR: &str = "cache";

    fn new_redis_client() -> redis::Client {
        let redis_client = redis::Client::open("redis://localhost:3001/")
            .expect("Failed to connect to redis server (test)");
        return redis_client;
    }

    fn get_file_all(path: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        fs::File::open(path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    fn file_not_exist(path: &str) -> bool {
        match fs::File::open(path) {
            Ok(_) => false,
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    return true;
                }
                false
            }
        }
    }

    macro_rules! new_lru_redis_cache {
        ($dir: expr, $size: expr, $redis_client: expr, $id: expr) => {
            LruRedisCache::new($dir, $size, $redis_client, $id)
        };
    }

    macro_rules! cache_put {
        ($cache: ident, $k: expr, $v: expr) => {
            $cache.put($k, $v).await;
        };
    }

    macro_rules! cache_get {
        ($cache: ident, $k: expr) => {
            $cache.get($k).await
        };
    }

    #[test]
    fn lru_prefix_key() {
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 1024, new_redis_client(), "prefix_key_test");
        assert_eq!(lru_cache.to_prefixed_key("April"), "prefix_key_test_April")
    }

    #[tokio::test]
    async fn test_cache_entry_set_success() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16 * 1024 * 1024,
            redis_client,
            "test_cache_entry_success"
        );
        let key = "answer";
        let cached_data = vec![42];
        let len = cached_data.len();
        cache_put!(lru_cache, "answer", cached_data.clone().into());
        let total_size_expected = len as u64;
        let total_size_actual: u64 = lru_cache.get_total_size();
        let cached_data_actual = get_file_all(&format!("{}/{}", TEST_CACHE_DIR, key));
        // metadata: size is 1, file content is the same
        assert_eq!(total_size_actual, total_size_expected);
        assert_eq!(&cached_data_actual, &cached_data);
    }

    #[tokio::test]
    async fn lru_cache_size_constraint() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16,
            redis_client,
            "lru_cache_size_constraint"
        );
        cache_put!(lru_cache, "tsu_ki", vec![0; 5].into());
        let total_size_actual: u64 = lru_cache.get_total_size();
        assert_eq!(total_size_actual, 5);
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, "kirei", vec![0; 11].into());
        let total_size_actual: u64 = lru_cache.get_total_size();
        assert_eq!(total_size_actual, 16);
        assert_eq!(
            get_file_all(&format!("{}/{}", TEST_CACHE_DIR, "tsu_ki")),
            vec![0; 5]
        );
        assert_eq!(
            get_file_all(&format!("{}/{}", TEST_CACHE_DIR, "kirei")),
            vec![0; 11]
        );
        // cache is full, evict tsu_ki
        cache_put!(lru_cache, "suki", vec![1; 4].into());
        assert_eq!(lru_cache.get_total_size(), 15);
        assert_eq!(
            file_not_exist(&format!("{}/{}", TEST_CACHE_DIR, "tsu_ki")),
            true
        );
        // evict both
        cache_put!(lru_cache, "deadbeef", vec![2; 16].into());
        assert_eq!(lru_cache.get_total_size(), 16);
        assert_eq!(
            file_not_exist(&format!("{}/{}", TEST_CACHE_DIR, "kirei")),
            true
        );
        assert_eq!(
            file_not_exist(&format!("{}/{}", TEST_CACHE_DIR, "suki")),
            true
        );
        assert_eq!(
            get_file_all(&format!("{}/{}", TEST_CACHE_DIR, "deadbeef")),
            vec![2; 16]
        );
    }

    #[tokio::test]
    async fn test_lru_cache_no_evict_recent() {
        let redis_client = new_redis_client();
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, redis_client, "lru_no_evict_recent");
        let key1 = "1二号去听经";
        let key2 = "2晚上住旅店";
        let key3 = "3三号去餐厅";
        let key4 = "4然后看电影";
        cache_put!(lru_cache, key1, vec![1].into());
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key2, vec![2].into());
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key3, vec![3].into());
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key4, evict key1
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key4, vec![4].into());
        assert!(cache_get!(lru_cache, key1).is_none());
        // assert
        assert_eq!(lru_cache.get_total_size(), 3);
        // get key2, update atime
        thread::sleep(time::Duration::from_secs(1));
        assert_eq!(cache_get!(lru_cache, key2).unwrap().to_vec().await, vec![2]);
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key1, evict key3
        thread::sleep(time::Duration::from_secs(1));
        cache_put!(lru_cache, key1, vec![11].into());
        assert_eq!(lru_cache.get_total_size(), 3);
        assert!(cache_get!(lru_cache, key3).is_none());
        assert_eq!(lru_cache.get_total_size(), 3);
    }

    #[tokio::test]
    async fn test_atime_updated_upon_access() {
        let redis_client = new_redis_client();
        let redis_client_tester = new_redis_client();
        let mut con = redis_client_tester.get_connection().unwrap();
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, redis_client, "atime_updated_upon_access");
        let key = "Shire";
        cache_put!(lru_cache, key, vec![0].into());
        let atime_before: i64 = con.hget(lru_cache.to_prefixed_key(key), "atime").unwrap();
        thread::sleep(time::Duration::from_secs(1));
        cache_get!(lru_cache, key);
        let atime_after: i64 = con.hget(lru_cache.to_prefixed_key(key), "atime").unwrap();
        assert_eq!(atime_before < atime_after, true);
    }

    #[tokio::test]
    async fn key_update_total_size() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            3,
            redis_client,
            "key_update_no_change_total_size"
        );
        let key = "Phantom";
        cache_put!(lru_cache, key, vec![0].into());
        assert_eq!(lru_cache.get_total_size(), 1);
        cache_put!(lru_cache, key, vec![0, 1].into());
        assert_eq!(lru_cache.get_total_size(), 2);
    }

    #[tokio::test]
    async fn lru_cache_isolation() {
        let lru_cache_1 = new_lru_redis_cache!(
            &format!("{}/{}", TEST_CACHE_DIR, "1"),
            3,
            new_redis_client(),
            "cache_isolation_1"
        );
        let lru_cache_2 = new_lru_redis_cache!(
            &format!("{}/{}", TEST_CACHE_DIR, "2"),
            3,
            new_redis_client(),
            "cache_isolation_2"
        );
        cache_put!(lru_cache_1, "1", vec![1].into());
        cache_put!(lru_cache_2, "2", vec![2].into());
        assert_eq!(lru_cache_1.get_total_size(), 1);
        assert_eq!(lru_cache_2.get_total_size(), 1);
        assert_eq!(
            cache_get!(lru_cache_1, "1").unwrap().to_vec().await,
            vec![1 as u8]
        );
        assert!(cache_get!(lru_cache_1, "2").is_none());
        assert!(cache_get!(lru_cache_2, "1").is_none());
        assert_eq!(
            cache_get!(lru_cache_2, "2").unwrap().to_vec().await,
            vec![2]
        );
    }

    #[tokio::test]
    async fn cache_stream_size_valid() {
        let lru_cache = new_lru_redis_cache!(TEST_CACHE_DIR, 3, new_redis_client(), "stream_cache");
        let bytes: Bytes = Bytes::from(vec![1, 1, 4]);
        let stream = stream::iter(vec![Ok(bytes.clone())]);
        let stream: Box<dyn Stream<Item = Result<Bytes>> + Send + Unpin> = Box::new(stream);
        cache_put!(lru_cache, "na tsu", CacheData::ByteStream(stream, Some(3)));
        let size = lru_cache.get_total_size();
        assert_eq!(size, 3);
    }
}
