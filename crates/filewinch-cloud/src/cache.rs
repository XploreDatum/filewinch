//! Bounded path → item cache with a time-to-live. Cloud drives address items by ID; resolving
//! `/a/b/c` means listing each parent, so recent lookups are kept. Every write invalidates the
//! affected path and everything under it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use filewinch_core::RPath;

pub(crate) struct PathCache<T> {
    inner: Mutex<Inner<T>>,
    capacity: usize,
    ttl: Duration,
}

struct Inner<T> {
    map: HashMap<RPath, Slot<T>>,
    /// Insertion order for eviction; stale stamps (re-inserted or removed paths) are skipped.
    order: VecDeque<(RPath, u64)>,
    next_stamp: u64,
}

struct Slot<T> {
    value: T,
    at: Instant,
    stamp: u64,
}

impl<T: Clone> PathCache<T> {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        PathCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                next_stamp: 0,
            }),
            capacity: capacity.max(1),
            ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn get(&self, path: &RPath) -> Option<T> {
        let mut g = self.lock();
        match g.map.get(path) {
            Some(s) if s.at.elapsed() < self.ttl => Some(s.value.clone()),
            Some(_) => {
                g.map.remove(path);
                None
            }
            None => None,
        }
    }

    pub(crate) fn put(&self, path: RPath, value: T) {
        let mut g = self.lock();
        let stamp = g.next_stamp;
        g.next_stamp += 1;
        g.order.push_back((path.clone(), stamp));
        g.map.insert(
            path,
            Slot {
                value,
                at: Instant::now(),
                stamp,
            },
        );
        while g.map.len() > self.capacity {
            let Some((p, s)) = g.order.pop_front() else {
                break;
            };
            if g.map.get(&p).is_some_and(|slot| slot.stamp == s) {
                g.map.remove(&p);
            }
        }
        // Keep the order queue from growing without bound under heavy re-insertion.
        if g.order.len() > self.capacity * 4 {
            let Inner { map, order, .. } = &mut *g;
            order.retain(|(p, s)| map.get(p).is_some_and(|slot| slot.stamp == *s));
        }
    }

    /// Forgets `path` and everything under it.
    pub(crate) fn invalidate(&self, path: &RPath) {
        let mut g = self.lock();
        g.map.retain(|p, _| !p.starts_with(path));
    }

    pub(crate) fn clear(&self) {
        let mut g = self.lock();
        g.map.clear();
        g.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_invalidates_subtrees() {
        let c = PathCache::new(3, Duration::from_secs(60));
        for p in ["/a", "/a/b", "/a/b/c", "/d"] {
            c.put(RPath::new(p), p.to_string());
        }
        assert_eq!(c.len(), 3);
        assert_eq!(c.get(&RPath::new("/a")), None, "oldest evicted");
        c.invalidate(&RPath::new("/a/b"));
        assert_eq!(c.get(&RPath::new("/a/b/c")), None);
        assert_eq!(c.get(&RPath::new("/d")).as_deref(), Some("/d"));
    }

    #[test]
    fn reinsert_keeps_newest_and_ttl_expires() {
        let c = PathCache::new(2, Duration::from_secs(60));
        c.put(RPath::new("/x"), 1);
        c.put(RPath::new("/x"), 2);
        c.put(RPath::new("/y"), 3);
        assert_eq!(c.get(&RPath::new("/x")), Some(2));
        let short = PathCache::new(2, Duration::from_millis(0));
        short.put(RPath::new("/x"), 1);
        assert_eq!(short.get(&RPath::new("/x")), None);
    }
}
