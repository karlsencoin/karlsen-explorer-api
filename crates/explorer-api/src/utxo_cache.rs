//! Short-lived cache of an address's full, sorted UTXO set, used only by the paged
//! form of `GET /addresses/{address}/utxos`.
//!
//! The node returns the whole set on every call; for the pool address that is
//! ~215k entries. Without a cache every page would rescan the UTXO index. Entries
//! live `ttl` (default 15 s): fresh enough for an explorer, and at most `capacity`
//! addresses are kept so memory stays bounded. Concurrent requests for the same
//! address share one node call.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::OnceCell;

use crate::node::proto::RpcUtxosByAddressesEntry;

type Set = Arc<Vec<RpcUtxosByAddressesEntry>>;

struct Slot {
    created: Instant,
    cell: Arc<OnceCell<Set>>,
}

pub struct UtxoCache {
    ttl: Duration,
    capacity: usize,
    slots: Mutex<HashMap<String, Slot>>,
}

impl Default for UtxoCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(15), 32)
    }
}

impl UtxoCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_fetch<F, Fut, E>(&self, address: &str, fetch: F) -> Result<Set, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<RpcUtxosByAddressesEntry>, E>>,
    {
        let cell = {
            let mut slots = self.slots.lock().expect("utxo cache lock");
            let now = Instant::now();
            slots.retain(|_, s| now.duration_since(s.created) < self.ttl);
            if !slots.contains_key(address) && slots.len() >= self.capacity {
                if let Some(oldest) = slots
                    .iter()
                    .min_by_key(|(_, s)| s.created)
                    .map(|(k, _)| k.clone())
                {
                    slots.remove(&oldest);
                }
            }
            slots
                .entry(address.to_string())
                .or_insert_with(|| Slot {
                    created: now,
                    cell: Arc::new(OnceCell::new()),
                })
                .cell
                .clone()
        };
        let set = cell
            .get_or_try_init(|| async { fetch().await.map(Arc::new) })
            .await?;
        Ok(set.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn one_fetch_per_address_within_ttl_and_refetch_after() {
        let cache = UtxoCache::new(Duration::from_millis(50), 4);
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(vec![RpcUtxosByAddressesEntry::default()])
        };
        cache.get_or_fetch("a", fetch).await.unwrap();
        cache.get_or_fetch("a", fetch).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(60)).await;
        cache.get_or_fetch("a", fetch).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn errors_are_not_cached_and_capacity_is_bounded() {
        let cache = UtxoCache::new(Duration::from_secs(60), 2);
        assert!(cache
            .get_or_fetch("a", || async { Err::<Vec<_>, _>("down") })
            .await
            .is_err());
        let ok = || async { Ok::<_, &str>(vec![]) };
        cache.get_or_fetch("a", ok).await.unwrap();
        cache.get_or_fetch("b", ok).await.unwrap();
        cache.get_or_fetch("c", ok).await.unwrap();
        assert_eq!(cache.slots.lock().unwrap().len(), 2);
    }
}
