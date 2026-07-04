//! Sharded LRU implementation.
//!
//! Each shard keeps a `HashMap` from key to `(value, charge, tick)` alongside a
//! `BTreeMap` from monotonically increasing tick to key that orders entries by
//! recency. A lookup or insert stamps the entry with a fresh tick and moves it to
//! the most-recent position; eviction repeatedly drops the entry with the
//! smallest tick until the shard is back within its charge budget. Both maps are
//! kept in lock-step so the recency index never references a missing key.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

struct Entry<V> {
    value: V,
    charge: usize,
    tick: u64,
}

struct Shard<K, V> {
    map: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
    usage: usize,
    capacity: usize,
    next_tick: u64,
}

impl<K, V> Shard<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new(capacity: usize) -> Self {
        Shard {
            map: HashMap::new(),
            order: BTreeMap::new(),
            usage: 0,
            capacity,
            next_tick: 0,
        }
    }

    fn touch(&mut self, key: &K) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        if let Some(entry) = self.map.get_mut(key) {
            self.order.remove(&entry.tick);
            entry.tick = tick;
            self.order.insert(tick, key.clone());
        }
        tick
    }

    fn get(&mut self, key: &K) -> Option<V> {
        if !self.map.contains_key(key) {
            return None;
        }
        self.touch(key);
        self.map.get(key).map(|e| e.value.clone())
    }

    fn insert(&mut self, key: K, value: V, charge: usize) {
        // Remove any prior entry for this key first.
        if let Some(old) = self.map.remove(&key) {
            self.order.remove(&old.tick);
            self.usage -= old.charge;
        }
        let tick = self.next_tick;
        self.next_tick += 1;
        self.order.insert(tick, key.clone());
        self.usage += charge;
        self.map.insert(key, Entry { value, charge, tick });
        self.evict_to_capacity();
    }

    fn evict_to_capacity(&mut self) {
        while self.usage > self.capacity {
            let victim_tick = match self.order.keys().next().copied() {
                Some(t) => t,
                None => break,
            };
            if let Some(victim_key) = self.order.remove(&victim_tick) {
                if let Some(entry) = self.map.remove(&victim_key) {
                    self.usage -= entry.charge;
                }
            }
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(entry) = self.map.remove(key) {
            self.order.remove(&entry.tick);
            self.usage -= entry.charge;
        }
    }
}

/// A thread-safe cache partitioned into independently-locked LRU shards.
pub struct ShardedCache<K, V> {
    shards: Vec<Mutex<Shard<K, V>>>,
    shard_mask: usize,
}

impl<K, V> ShardedCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Create a cache holding at most `total_capacity` charge units, split
    /// evenly across `2^shard_bits` shards.
    pub fn new(total_capacity: usize, shard_bits: u32) -> Self {
        let num_shards = 1usize << shard_bits;
        let per_shard = total_capacity.div_ceil(num_shards).max(1);
        let shards = (0..num_shards)
            .map(|_| Mutex::new(Shard::new(per_shard)))
            .collect();
        ShardedCache {
            shards,
            shard_mask: num_shards - 1,
        }
    }

    fn shard_index(&self, key: &K) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) & self.shard_mask
    }

    /// Look up `key`, promoting it to most-recently-used on a hit.
    pub fn get(&self, key: &K) -> Option<V> {
        let idx = self.shard_index(key);
        self.shards[idx].lock().unwrap().get(key)
    }

    /// Insert or overwrite `key`, charging `charge` units against the budget.
    pub fn insert(&self, key: K, value: V, charge: usize) {
        let idx = self.shard_index(&key);
        self.shards[idx].lock().unwrap().insert(key, value, charge);
    }

    /// Evict `key` if present.
    pub fn remove(&self, key: &K) {
        let idx = self.shard_index(key);
        self.shards[idx].lock().unwrap().remove(key);
    }

    /// Total charge currently held across all shards.
    pub fn usage(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().usage).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_after_insert() {
        let c: ShardedCache<String, u32> = ShardedCache::new(100, 2);
        c.insert("a".to_string(), 1, 1);
        c.insert("b".to_string(), 2, 1);
        assert_eq!(c.get(&"a".to_string()), Some(1));
        assert_eq!(c.get(&"b".to_string()), Some(2));
        assert_eq!(c.get(&"c".to_string()), None);
    }

    #[test]
    fn evicts_when_over_capacity() {
        // One shard, capacity 3 units.
        let c: ShardedCache<u32, u32> = ShardedCache::new(3, 0);
        for i in 0..3 {
            c.insert(i, i * 10, 1);
        }
        // Touch 0 and 2 so 1 becomes least-recently-used.
        assert_eq!(c.get(&0), Some(0));
        assert_eq!(c.get(&2), Some(20));
        // Insert a 4th entry; something must have been evicted.
        c.insert(3, 30, 1);
        assert!(c.usage() <= 3);
        // The most-recently-used survivors are still present.
        assert_eq!(c.get(&3), Some(30));
    }

    #[test]
    fn overwrite_updates_value_and_charge() {
        let c: ShardedCache<u32, String> = ShardedCache::new(100, 1);
        c.insert(1, "small".to_string(), 1);
        c.insert(1, "bigger-value".to_string(), 3);
        assert_eq!(c.get(&1), Some("bigger-value".to_string()));
    }

    #[test]
    fn remove_frees_charge() {
        let c: ShardedCache<u32, u32> = ShardedCache::new(100, 0);
        c.insert(1, 1, 5);
        assert_eq!(c.usage(), 5);
        c.remove(&1);
        assert_eq!(c.usage(), 0);
        assert_eq!(c.get(&1), None);
    }
}
