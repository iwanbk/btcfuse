use std::path::Path;
use std::time::Duration;

use moka::sync::Cache as MokaCache;
use sled::Db;

use crate::error::BtcFuseError;

const VOLATILE_MAX_CAPACITY: u64 = 10_000;
// Global TTL for volatile entries. Per-key TTL (30s vs 60s, see CLAUDE.md table)
// is owned by the fetch_cached helper added in the Step 3 follow-up; this
// module keeps only get/set primitives.
const VOLATILE_TTL: Duration = Duration::from_mins(1);

pub struct Cache {
    volatile: MokaCache<String, Vec<u8>>,
    persistent: Db,
}

impl Cache {
    pub fn new(sled_path: &Path) -> Result<Self, BtcFuseError> {
        if let Some(parent) = sled_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        }
        let persistent = sled::open(sled_path).map_err(|e| BtcFuseError::Cache(e.to_string()))?;
        let volatile = MokaCache::builder()
            .max_capacity(VOLATILE_MAX_CAPACITY)
            .time_to_live(VOLATILE_TTL)
            .build();
        Ok(Self {
            volatile,
            persistent,
        })
    }

    #[allow(dead_code)]
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        if let Some(v) = self.volatile.get(key) {
            return Some(v);
        }
        self.persistent.get(key).ok().flatten().map(|v| v.to_vec())
    }

    pub fn set(&self, key: &str, value: &[u8], ttl: Duration) {
        if ttl == Duration::MAX {
            let _ = self.persistent.insert(key, value);
        } else {
            self.volatile.insert(key.to_owned(), value.to_vec());
        }
    }
}
