//! A bounded least-recently-used map, weighed: what the caches of
//! immutable things (a commit's listing, its last-commit map) are kept
//! in. Its weight never passes its bound after an insert; one entry
//! heavier than the bound is not kept at all.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

pub struct Lru<K, V> {
    map: HashMap<K, (V, u64, u64)>,
    /// (key, stamp) in use order; stale stamps are skipped.
    order: VecDeque<(K, u64)>,
    weight: u64,
    max: u64,
    stamp: u64,
}

impl<K: Hash + Eq + Clone, V: Clone> Lru<K, V> {
    pub fn new(max: u64) -> Lru<K, V> {
        assert!(max > 0);
        Lru { map: HashMap::new(), order: VecDeque::new(), weight: 0, max, stamp: 0 }
    }

    fn touch(&mut self, k: &K) -> u64 {
        self.stamp += 1;
        self.order.push_back((k.clone(), self.stamp));
        // The order queue holds a stale stamp per touch; compact it when it
        // grows past four times the live entries, so it stays bounded.
        if self.order.len() > 4 * self.map.len() + 64 {
            let map = &self.map;
            let s = self.stamp;
            self.order.retain(|(k2, st)| map.get(k2).is_some_and(|e| e.2 == *st) || *st == s);
        }
        self.stamp
    }

    pub fn get(&mut self, k: &K) -> Option<V> {
        if !self.map.contains_key(k) {
            return None;
        }
        let s = self.touch(k);
        let e = self.map.get_mut(k).expect("present");
        e.2 = s;
        Some(e.0.clone())
    }

    pub fn insert(&mut self, k: K, v: V, weight: u64) {
        if let Some((_, w, _)) = self.map.remove(&k) {
            self.weight -= w;
        }
        if weight > self.max {
            return;
        }
        let s = self.touch(&k);
        self.map.insert(k, (v, weight, s));
        self.weight += weight;
        // Bounded by the entries: each pass pops one.
        while self.weight > self.max {
            let (old, st) = self.order.pop_front().expect("a heavy map has entries in order");
            if self.map.get(&old).is_some_and(|e| e.2 == st) {
                let (_, w, _) = self.map.remove(&old).expect("present");
                self.weight -= w;
            }
        }
        assert!(self.weight <= self.max);
    }

    pub fn weight(&self) -> u64 {
        self.weight
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: the least recently used goes first, the weight stays within
    // its bound, a too-heavy entry is not kept, and the order queue stays
    // bounded under many touches. Method: inserts and gets.
    #[test]
    fn evicts_by_use_and_weight() {
        let mut l = Lru::new(10);
        l.insert("a", 1, 4);
        l.insert("b", 2, 4);
        assert_eq!(l.get(&"a"), Some(1));
        l.insert("c", 3, 4);
        assert_eq!(l.get(&"b"), None, "b was the least recently used");
        assert_eq!((l.get(&"a"), l.get(&"c"), l.weight()), (Some(1), Some(3), 8));
        l.insert("huge", 9, 11);
        assert_eq!(l.get(&"huge"), None);
        l.insert("a", 5, 2);
        assert_eq!((l.get(&"a"), l.weight()), (Some(5), 6));
        for _ in 0..10_000 {
            l.get(&"a");
        }
        assert!(l.order.len() <= 4 * l.len() + 65);
        assert_eq!(l.get(&"c"), Some(3));
    }
}
