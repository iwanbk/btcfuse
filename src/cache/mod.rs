use std::time::Duration;

use moka::sync::Cache as MokaCache;
use sled::Db;

use crate::error::BtcFuseError;

pub const TTL_CHAIN_HEIGHT: Duration = Duration::from_secs(30);
pub const TTL_BLOCK_LATEST: Duration = Duration::from_mins(1);
pub const TTL_CHAIN_FEE: Duration = Duration::from_secs(30);
pub const TTL_ADDRESS: Duration = Duration::from_secs(30);
pub const TTL_NEGATIVE: Duration = Duration::from_secs(30);
pub const TTL_TX_UNCONFIRMED: Duration = Duration::from_mins(1);

// Moka cache stores (bytes, ttl) so the expiry policy can read TTL per entry.
type Entry = (Vec<u8>, Duration);

struct TtlExpiry;

impl moka::Expiry<String, Entry> for TtlExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Entry,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(value.1)
    }
}

pub struct Cache {
    moka: MokaCache<String, Entry>,
    sled: Db,
}

impl Cache {
    pub fn new(sled_path: &str) -> Result<Self, BtcFuseError> {
        let moka = MokaCache::builder()
            .max_capacity(10_000)
            .expire_after(TtlExpiry)
            .build();
        let sled = sled::open(sled_path).map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        Ok(Self { moka, sled })
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.moka.get(key).map(|(bytes, _)| bytes)
    }

    pub fn set(&self, key: &str, val: Vec<u8>, ttl: Duration) {
        self.moka.insert(key.to_string(), (val, ttl));
    }

    pub fn get_persistent(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>, BtcFuseError> {
        let tree = self
            .sled
            .open_tree(bucket)
            .map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        tree.get(key)
            .map_err(|e| BtcFuseError::Cache(e.to_string()))
            .map(|opt| opt.map(|iv| iv.to_vec()))
    }

    pub fn set_persistent(&self, bucket: &str, key: &str, val: &[u8]) -> Result<(), BtcFuseError> {
        let tree = self
            .sled
            .open_tree(bucket)
            .map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        tree.insert(key, val)
            .map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        Ok(())
    }

    pub fn set_negative(&self, path: &str) {
        self.set(&format!("negative:{path}"), vec![], TTL_NEGATIVE);
    }

    pub fn is_negative(&self, path: &str) -> bool {
        self.get(&format!("negative:{path}")).is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use tempfile::TempDir;

    use super::*;

    fn temp_dir(tag: &str) -> TempDir {
        tempfile::Builder::new()
            .prefix(&format!("btcfuse_{tag}_"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn moka_ttl_expiry() {
        let dir = temp_dir("ttl");
        let cache = Cache::new(dir.path().to_str().unwrap()).unwrap();
        cache.set("k", b"v".to_vec(), Duration::from_millis(100));
        assert!(cache.get("k").is_some());
        thread::sleep(Duration::from_millis(300));
        assert!(cache.get("k").is_none());
    }

    #[test]
    fn sled_survives_restart() {
        let dir = temp_dir("restart");
        let path = dir.path().to_str().unwrap().to_string();
        {
            let cache = Cache::new(&path).unwrap();
            cache.set_persistent("b", "key", b"value").unwrap();
            // sled flushes on Drop
        }
        let cache2 = Cache::new(&path).unwrap();
        assert_eq!(
            cache2.get_persistent("b", "key").unwrap(),
            Some(b"value".to_vec())
        );
    }

    #[test]
    fn negative_cache_hit() {
        let dir = temp_dir("neg");
        let cache = Cache::new(dir.path().to_str().unwrap()).unwrap();
        cache.set_negative("/blocks/999");
        assert!(cache.is_negative("/blocks/999"));
        assert!(!cache.is_negative("/blocks/888"));
    }
}
