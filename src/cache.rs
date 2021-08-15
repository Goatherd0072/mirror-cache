use metrics::{histogram, increment_counter, register_histogram};
use redis::Commands;
use std::fs;
use std::io::prelude::*;
use std::marker::Send;
use std::vec::Vec;

use crate::metric;
use crate::models;
use crate::util;

use bytes::Bytes;

pub trait CachePolicy: Sync + Send {
    fn put(&self, key: &str, entry: Bytes);
    fn get(&self, key: &str) -> Option<Bytes>;
}

#[derive(Clone)]
pub struct LruRedisCache {
    root_dir: String,
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
        println!("registered: {}", Self::get_metric_key(id));
        Self {
            root_dir: String::from(root_dir),
            size_limit,
            redis_client,
            id: id.to_string(),
        }
    }

    pub fn to_fs_path(&self, cache_key: &str) -> String {
        let cache_key = &cache_key[self.id.len() + 1..];
        format!("{}/{}", self.root_dir, cache_key)
    }

    fn get_total_size(&self) -> u64 {
        let key = self.total_size_key();
        let mut con = self.redis_client.get_connection().unwrap();
        let size = con.get::<&str, Option<u64>>(&key).unwrap().unwrap_or(0);
        histogram!(Self::get_metric_key(&self.id), size as f64);
        println!("logged: {} {}", Self::get_metric_key(&self.id), size as f64);
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

impl CachePolicy for LruRedisCache {
    /**
     * put a cache entry with given `key` as key and `entry` as value
     * An entry larger than the size limit of the current cache (self) is ignored.
     * If the size limit is exceeded after putting the entry, LRU eviction will run.
     * This function handles both local FS data and redis metadata.
     */
    fn put(&self, key: &str, entry: Bytes) {
        let key = &self.to_prefixed_key(key);
        // eviction policy
        let file_size = entry.len() as u64;
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();

        if file_size > self.size_limit {
            info!("skip cache for {}, because its size exceeds the limit", key);
        }
        // evict cache entry if necessary
        let _tx_result = redis::transaction(
            &mut sync_con,
            &[key, &self.total_size_key(), &self.entries_zlist_key()],
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
                        let path = self.to_fs_path(&f);
                        match fs::remove_file(&path) {
                            Ok(_) => {
                                increment_counter!(metric::CNT_RM_FILES);
                                info!("LRU cache removed {}", &path);
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
        let data_to_write = entry;
        let fs_path = self.to_fs_path(key);
        let (parent_dirs, _cache_file_name) = util::split_dirs(&fs_path);
        fs::create_dir_all(parent_dirs).unwrap();
        let mut f = fs::File::create(fs_path).unwrap();
        f.write_all(&data_to_write).unwrap();
        let entry = &CacheEntry::new(&key, data_to_write.len() as u64);
        let _redis_resp_str = models::set_lru_cache_entry(
            &mut sync_con,
            &key,
            entry,
            &self.total_size_key(),
            &self.entries_zlist_key(),
        );
        trace!("CACHE SET {} -> {:?}", &key, entry);
    }

    fn get(&self, key: &str) -> Option<Bytes> {
        let key = &self.to_prefixed_key(key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        let cache_result = models::get_cache_entry(&mut sync_con, key).unwrap();
        if let Some(_cache_entry) = &cache_result {
            // cache hit
            // update cache entry in db
            let new_atime = util::now();
            match models::update_cache_entry_atime(
                &mut sync_con,
                key,
                new_atime,
                &self.entries_zlist_key(),
            ) {
                Ok(_) => {}
                Err(e) => {
                    info!("Failed to update cache entry atime: {}", e);
                }
            }
            let cached_file_path = self.to_fs_path(key);
            let file_content = match fs::read(cached_file_path) {
                Ok(data) => data,
                Err(_) => vec![],
            };
            if file_content.len() > 0 {
                trace!("CACHE GET [HIT] {} -> {:?} ", key, &cache_result);
                return Some(Bytes::from(file_content));
            }
        };
        trace!("CACHE GET [MISS] {} -> {:?} ", key, &cache_result);
        None
    }
}

/**
 *
 * TtlRedisCache is a simple cache policy that expire an existing cache entry
 * within the given TTL. The expiration is supported by redis.
 */
pub struct TtlRedisCache {
    pub root_dir: String,
    pub ttl: u64, // cache entry ttl in seconds
    redis_client: redis::Client,
}

impl TtlRedisCache {
    pub fn new(root_dir: &str, ttl: u64, redis_client: redis::Client) -> Self {
        let cloned_client = redis_client.clone();
        let cloned_root_dir = String::from(root_dir);
        std::thread::spawn(move || {
            debug!("TTL expiration listener is created!");
            loop {
                let mut con = cloned_client.get_connection().unwrap();
                let mut pubsub = con.as_pubsub();
                // TODO: subscribe only current cache key pattern
                match pubsub.psubscribe("__keyevent*__:expired") {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to psubscribe: {}", e);
                        continue;
                    }
                }
                loop {
                    match pubsub.get_message() {
                        Ok(msg) => {
                            let payload: String = msg.get_payload().unwrap();
                            let cache_key = Self::from_redis_key(&cloned_root_dir, &payload);
                            trace!(
                                "channel '{}': {}, pkg: {}",
                                msg.get_channel_name(),
                                payload,
                                cache_key
                            );
                            let file_path = Self::to_fs_path(&cloned_root_dir, &cache_key);
                            match fs::remove_file(&file_path) {
                                Ok(_) => {
                                    increment_counter!(metric::CNT_RM_FILES);
                                    info!("TTL cache removed {}", file_path);
                                }
                                Err(e) => {
                                    if e.kind() == std::io::ErrorKind::NotFound {
                                        // warn!("Failed to remove {}: {}", &file_path, e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to get_message: {}", e);
                        }
                    }
                }
            }
        });
        Self {
            root_dir: String::from(root_dir),
            ttl,
            redis_client,
        }
    }

    pub fn to_redis_key(root_dir: &str, cache_key: &str) -> String {
        format!("ttl_redis_cache/{}/{}", root_dir, cache_key)
    }
    pub fn from_redis_key(root_dir: &str, key: &str) -> String {
        String::from(&key[16 + root_dir.len() + 1..])
    }

    pub fn to_fs_path(root_dir: &str, cache_key: &str) -> String {
        format!("{}/{}", root_dir, cache_key)
    }
}

impl CachePolicy for TtlRedisCache {
    fn put(&self, key: &str, entry: Bytes) {
        let redis_key = Self::to_redis_key(&self.root_dir, key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        let data_to_write = entry;
        let fs_path = Self::to_fs_path(&self.root_dir, &key);
        let (parent_dirs, _cached_filename) = util::split_dirs(&fs_path);
        fs::create_dir_all(parent_dirs).unwrap();
        let mut f = fs::File::create(fs_path).unwrap();
        f.write_all(&data_to_write).unwrap();
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

    fn get(&self, key: &str) -> Option<Bytes> {
        let redis_key = Self::to_redis_key(&self.root_dir, key);
        let mut sync_con = models::get_sync_con(&self.redis_client).unwrap();
        match models::get(&mut sync_con, &redis_key) {
            Ok(res) => match res {
                Some(_) => {
                    let cached_file_path = Self::to_fs_path(&self.root_dir, &key);
                    let file_content = match fs::read(cached_file_path) {
                        Ok(data) => data,
                        Err(_) => vec![],
                    };
                    if file_content.len() > 0 {
                        trace!("GET {} [HIT]", key);
                        return Some(Bytes::from(file_content));
                    }
                    None
                }
                None => None,
            },
            Err(e) => {
                info!("get cache entry key={} failed: {}", key, e);
                None
            }
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

impl CachePolicy for NoCache {
    fn put(&self, _key: &str, _entry: Bytes) {}
    fn get(&self, _key: &str) -> Option<Bytes> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::thread;
    use std::time;

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
        ($dir: expr, $size: expr, $redis_client: expr, $id: expr ) => {
            LruRedisCache::new($dir, $size, $redis_client, $id)
        };
    }
    #[test]
    fn lru_prefix_key() {
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 1024, new_redis_client(), "prefix_key_test");
        assert_eq!(lru_cache.to_prefixed_key("April"), "prefix_key_test_April")
    }

    #[test]
    fn test_cache_entry_success() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16 * 1024 * 1024,
            redis_client,
            "test_cache_entry_success"
        );
        let key = "answer";
        let cached_data = vec![42];
        lru_cache.put("answer", cached_data.as_slice().into());
        let total_size_expected = cached_data.len() as u64;
        let total_size_actual: u64 = lru_cache.get_total_size();
        let cached_data_actual = get_file_all(&format!("{}/{}", TEST_CACHE_DIR, key));
        // metadata: size is 1, file content is the same
        assert_eq!(total_size_actual, total_size_expected);
        assert_eq!(&cached_data_actual, &cached_data);
    }

    #[test]
    fn test_lru_cache_size_constraint() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            16,
            redis_client,
            "lru_cache_size_constraint"
        );
        lru_cache.put("tsu_ki", vec![0; 5]);
        let total_size_actual: u64 = lru_cache.get_total_size();
        assert_eq!(total_size_actual, 5);
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.put("kirei", vec![0; 11]);
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
        lru_cache.put("suki", vec![1; 4]);
        assert_eq!(lru_cache.get_total_size(), 15);
        assert_eq!(
            file_not_exist(&format!("{}/{}", TEST_CACHE_DIR, "tsu_ki")),
            true
        );
        // evict both
        lru_cache.put("deadbeef", vec![2; 16]);
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

    #[test]
    fn test_lru_cache_no_evict_recent() {
        let redis_client = new_redis_client();
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, redis_client, "lru_no_evict_recent");
        let key1 = "1二号去听经";
        let key2 = "2晚上住旅店";
        let key3 = "3三号去餐厅";
        let key4 = "4然后看电影";
        lru_cache.put(key1, vec![1]);
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.put(key2, vec![2]);
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.put(key3, vec![3]);
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key4, evict key1
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.put(key4, vec![4]);
        assert_eq!(lru_cache.get(key1), None);
        assert_eq!(lru_cache.get_total_size(), 3);
        // get key2, update atime
        thread::sleep(time::Duration::from_secs(1));
        assert_eq!(lru_cache.get(key2), Some(vec![2]));
        assert_eq!(lru_cache.get_total_size(), 3);
        // set key1, evict key3
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.put(key1, vec![11]);
        assert_eq!(lru_cache.get_total_size(), 3);
        assert_eq!(lru_cache.get(key3), None);
        assert_eq!(lru_cache.get_total_size(), 3);
    }

    #[test]
    // #[serial]
    fn test_atime_updated_upon_access() {
        let redis_client = new_redis_client();
        let redis_client_tester = new_redis_client();
        let mut con = redis_client_tester.get_connection().unwrap();
        let lru_cache =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, redis_client, "atime_updated_upon_access");
        let key = "Shire";
        lru_cache.put(key, vec![0]);
        let atime_before: i64 = con.hget(lru_cache.to_prefixed_key(key), "atime").unwrap();
        thread::sleep(time::Duration::from_secs(1));
        lru_cache.get(key);
        let atime_after: i64 = con.hget(lru_cache.to_prefixed_key(key), "atime").unwrap();
        assert_eq!(atime_before < atime_after, true);
    }

    #[test]
    fn key_update_total_size() {
        let redis_client = new_redis_client();
        let lru_cache = new_lru_redis_cache!(
            TEST_CACHE_DIR,
            3,
            redis_client,
            "key_update_no_change_total_size"
        );
        let key = "Phantom";
        lru_cache.put(key, vec![0]);
        assert_eq!(lru_cache.get_total_size(), 1);
        lru_cache.put(key, vec![0, 1]);
        assert_eq!(lru_cache.get_total_size(), 2);
    }

    #[test]
    fn lru_cache_isolation() {
        let lru_cache_1 =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, new_redis_client(), "cache_isolation_1");
        let lru_cache_2 =
            new_lru_redis_cache!(TEST_CACHE_DIR, 3, new_redis_client(), "cache_isolation_2");
        lru_cache_1.put("1", vec![1]);
        lru_cache_2.put("2", vec![2]);
        assert_eq!(lru_cache_1.get_total_size(), 1);
        assert_eq!(lru_cache_2.get_total_size(), 1);
        assert_eq!(lru_cache_1.get("1"), Some(vec![1]));
        assert_eq!(lru_cache_1.get("2"), None);
        assert_eq!(lru_cache_2.get("1"), None);
        assert_eq!(lru_cache_2.get("2"), Some(vec![2]));
    }
}
