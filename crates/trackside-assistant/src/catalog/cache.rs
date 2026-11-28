//! Small in-memory caches for the catalog. Nothing here is written to disk.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

use tokio::time::Instant;
use trackside_core::CatalogArtId;

struct Entry<V> {
    value: V,
    expires: Instant,
    weight: usize,
    used: u64,
}

/// Values that expire, bounded by count and by a total weight (items held). When either
/// bound is passed, expired entries go first, then the least recently used.
pub(crate) struct TtlCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    max_entries: usize,
    max_weight: usize,
    weight: usize,
    tick: u64,
}

impl<K: Hash + Eq + Clone, V: Clone> TtlCache<K, V> {
    pub(crate) fn new(max_entries: usize, max_weight: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
            max_weight: max_weight.max(1),
            weight: 0,
            tick: 0,
        }
    }

    pub(crate) fn get(&mut self, key: &K, now: Instant) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        match self.entries.get_mut(key) {
            Some(entry) if entry.expires > now => {
                entry.used = tick;
                Some(entry.value.clone())
            }
            Some(_) => {
                self.remove(key);
                None
            }
            None => None,
        }
    }

    /// Keeps `value` for `ttl`. A value heavier than the whole cache is not kept.
    pub(crate) fn put(&mut self, key: K, value: V, weight: usize, ttl: Duration, now: Instant) {
        if weight > self.max_weight {
            return;
        }
        self.remove(&key);
        self.tick += 1;
        self.weight += weight;
        self.entries.insert(
            key,
            Entry {
                value,
                expires: now + ttl,
                weight,
                used: self.tick,
            },
        );
        if self.entries.len() > self.max_entries || self.weight > self.max_weight {
            let expired: Vec<K> = self
                .entries
                .iter()
                .filter(|(_, e)| e.expires <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for key in expired {
                self.remove(&key);
            }
        }
        while self.entries.len() > self.max_entries || self.weight > self.max_weight {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(entry) = self.entries.remove(key) {
            self.weight -= entry.weight;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn weight(&self) -> usize {
        self.weight
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.weight = 0;
    }
}

/// The image addresses `m` ids were derived from, in two generations: when the current one
/// fills, it becomes the previous one and the oldest are forgotten. A lookup moves an
/// address back into the current generation.
pub(crate) struct ArtBook {
    current: HashMap<CatalogArtId, String>,
    previous: HashMap<CatalogArtId, String>,
    generation: usize,
}

impl ArtBook {
    pub(crate) fn new(per_generation: usize) -> Self {
        Self {
            current: HashMap::new(),
            previous: HashMap::new(),
            generation: per_generation.max(1),
        }
    }

    pub(crate) fn remember(&mut self, id: CatalogArtId, source: String) {
        if self.current.len() >= self.generation && !self.current.contains_key(&id) {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(id, source);
    }

    pub(crate) fn get(&mut self, id: &CatalogArtId) -> Option<String> {
        if let Some(source) = self.current.get(id) {
            return Some(source.clone());
        }
        let source = self.previous.remove(id)?;
        self.remember(id.clone(), source.clone());
        Some(source)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.current.len() + self.previous.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_expire() {
        let start = Instant::now();
        let mut cache = TtlCache::new(10, 100);
        cache.put("a", 1, 1, Duration::from_secs(5), start);
        assert_eq!(cache.get(&"a", start + Duration::from_secs(4)), Some(1));
        assert_eq!(cache.get(&"a", start + Duration::from_secs(5)), None);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn the_least_recently_used_go_first_past_either_bound() {
        let now = Instant::now();
        let ttl = Duration::from_mins(1);
        let mut cache = TtlCache::new(2, 100);
        cache.put("a", 1, 1, ttl, now);
        cache.put("b", 2, 1, ttl, now);
        assert_eq!(cache.get(&"a", now), Some(1));
        cache.put("c", 3, 1, ttl, now);
        assert_eq!(cache.get(&"b", now), None, "b was used least recently");
        assert_eq!(cache.len(), 2);

        let mut cache = TtlCache::new(100, 10);
        cache.put("a", 1, 6, ttl, now);
        cache.put("b", 2, 6, ttl, now);
        assert_eq!(cache.weight(), 6);
        assert_eq!(cache.get(&"a", now), None);
        cache.put("huge", 3, 11, ttl, now);
        assert_eq!(cache.get(&"huge", now), None, "never kept");
        assert_eq!(cache.get(&"b", now), Some(2));
    }

    #[test]
    fn the_art_book_forgets_the_oldest_generation() {
        let mut book = ArtBook::new(2);
        let id = |n: &str| CatalogArtId::derive(n);
        book.remember(id("1"), String::from("one"));
        book.remember(id("2"), String::from("two"));
        book.remember(id("3"), String::from("three"));
        assert_eq!(book.get(&id("1")).as_deref(), Some("one"));
        book.remember(id("4"), String::from("four"));
        book.remember(id("5"), String::from("five"));
        assert!(book.len() <= 4);
        assert_eq!(book.get(&id("2")), None, "forgotten");
        assert_eq!(book.get(&id("5")).as_deref(), Some("five"));
    }
}
