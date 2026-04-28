use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};

use crate::{cache::Cache, error::BtcFuseError};

/// Lookup order: negative cache → moka → sled (populate moka on hit) → `rpc_fn` → store.
///
/// `item_height = None` means mempool (unconfirmed); always stored in moka.
/// `item_height = Some(h)` where `latest_height - h >= 6` → stored in sled permanently.
/// Otherwise stored in moka with `mem_ttl`.
///
/// `latest_height` must come from the caller — this function never calls `get_block_count`.
pub fn fetch_cached<T, F>(
    cache: &Cache,
    mem_key: &str,
    persist_bucket: &str,
    latest_height: u64,
    item_height: Option<u64>,
    mem_ttl: Duration,
    rpc_fn: F,
) -> Result<T, BtcFuseError>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Result<T, BtcFuseError>,
{
    if cache.is_negative(mem_key) {
        return Err(BtcFuseError::NotFound(mem_key.to_string()));
    }

    if let Some(bytes) = cache.get(mem_key) {
        return bincode::deserialize(&bytes).map_err(|e| BtcFuseError::Cache(e.to_string()));
    }

    let is_finalized = item_height.is_some_and(|h| latest_height.saturating_sub(h) >= 6);

    if is_finalized {
        if let Some(bytes) = cache.get_persistent(persist_bucket, mem_key)? {
            cache.set(mem_key, bytes.clone(), mem_ttl);
            return bincode::deserialize(&bytes).map_err(|e| BtcFuseError::Cache(e.to_string()));
        }
    }

    match rpc_fn() {
        Ok(value) => {
            let bytes =
                bincode::serialize(&value).map_err(|e| BtcFuseError::Cache(e.to_string()))?;
            if is_finalized {
                cache.set_persistent(persist_bucket, mem_key, &bytes)?;
            } else {
                cache.set(mem_key, bytes, mem_ttl);
            }
            Ok(value)
        }
        Err(BtcFuseError::NotFound(_)) => {
            cache.set_negative(mem_key);
            Err(BtcFuseError::NotFound(mem_key.to_string()))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;
    use crate::cache::Cache;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Val {
        n: u64,
    }

    fn temp_cache(tag: &str) -> (Cache, TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("btcfuse_fetch_{tag}_"))
            .tempdir()
            .unwrap();
        let cache = Cache::new(dir.path().to_str().unwrap()).unwrap();
        (cache, dir)
    }

    #[test]
    fn stores_to_sled_when_finalized() {
        let (cache, _dir) = temp_cache("sled");
        let val = fetch_cached(
            &cache,
            "block:abc",
            "blocks",
            100,
            Some(90), // 10 confs >= 6
            Duration::from_secs(60),
            || Ok(Val { n: 42 }),
        )
        .unwrap();
        assert_eq!(val, Val { n: 42 });
        assert!(cache
            .get_persistent("blocks", "block:abc")
            .unwrap()
            .is_some());
        // Not in moka (finalized path skips moka write)
        assert!(cache.get("block:abc").is_none());
    }

    #[test]
    fn stores_to_moka_when_not_finalized() {
        let (cache, _dir) = temp_cache("moka_conf");
        let val = fetch_cached(
            &cache,
            "block:abc",
            "blocks",
            100,
            Some(98), // 2 confs < 6
            Duration::from_secs(60),
            || Ok(Val { n: 7 }),
        )
        .unwrap();
        assert_eq!(val, Val { n: 7 });
        assert!(cache.get("block:abc").is_some());
        assert!(cache
            .get_persistent("blocks", "block:abc")
            .unwrap()
            .is_none());
    }

    #[test]
    fn stores_to_moka_when_mempool() {
        let (cache, _dir) = temp_cache("moka_mem");
        let val = fetch_cached(
            &cache,
            "tx:ff00",
            "txs",
            100,
            None, // mempool
            Duration::from_secs(60),
            || Ok(Val { n: 99 }),
        )
        .unwrap();
        assert_eq!(val, Val { n: 99 });
        assert!(cache.get("tx:ff00").is_some());
        assert!(cache.get_persistent("txs", "tx:ff00").unwrap().is_none());
    }

    #[test]
    fn negative_cache_prevents_rpc() {
        let (cache, _dir) = temp_cache("neg");
        cache.set_negative("block:missing");
        let result = fetch_cached::<Val, _>(
            &cache,
            "block:missing",
            "blocks",
            100,
            Some(90),
            Duration::from_secs(60),
            || panic!("rpc_fn must not be called"),
        );
        assert!(matches!(result, Err(BtcFuseError::NotFound(_))));
    }

    #[test]
    fn not_found_sets_negative_cache() {
        let (cache, _dir) = temp_cache("notfound");
        let result = fetch_cached::<Val, _>(
            &cache,
            "block:gone",
            "blocks",
            100,
            Some(90),
            Duration::from_secs(60),
            || Err(BtcFuseError::NotFound("block:gone".into())),
        );
        assert!(matches!(result, Err(BtcFuseError::NotFound(_))));
        assert!(cache.is_negative("block:gone"));
    }

    #[test]
    fn rpc_error_propagates_without_negative_cache() {
        let (cache, _dir) = temp_cache("rpc_err");
        let result = fetch_cached::<Val, _>(
            &cache,
            "block:err",
            "blocks",
            100,
            Some(90),
            Duration::from_secs(60),
            || Err(BtcFuseError::Rpc("connection refused".into())),
        );
        assert!(matches!(result, Err(BtcFuseError::Rpc(_))));
        // Must NOT poison the negative cache — block may exist, just unreachable
        assert!(!cache.is_negative("block:err"));
    }

    #[test]
    fn sled_hit_populates_moka() {
        let (cache, _dir) = temp_cache("sled_hit");
        let bytes = bincode::serialize(&Val { n: 5 }).unwrap();
        cache.set_persistent("blocks", "block:old", &bytes).unwrap();

        let val: Val = fetch_cached(
            &cache,
            "block:old",
            "blocks",
            100,
            Some(90),
            Duration::from_secs(60),
            || panic!("rpc_fn must not be called"),
        )
        .unwrap();
        assert_eq!(val, Val { n: 5 });
        assert!(cache.get("block:old").is_some());
    }
}
