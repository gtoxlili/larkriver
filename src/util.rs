//! Cross-cutting utilities: hash map aliases, JSON helpers, RNG.
//!
//! `FoldHashMap` / `FoldHashSet` swap std's siphash for [`foldhash`] —
//! a non-cryptographic hash that's ~2× faster on modern CPUs and is the
//! direction `hashbrown` is moving for its default hasher.
//!
//! The aliases let us keep `let m = FoldHashMap::default();` ergonomics
//! while still being clearly typed at the boundaries.

pub type FoldHashMap<K, V> =
    hashbrown::HashMap<K, V, foldhash::fast::RandomState>;
pub type FoldHashSet<K> =
    hashbrown::HashSet<K, foldhash::fast::RandomState>;

/// Fast non-DoS-resistant `HashMap` for purely-internal collections (per-hand
/// histograms, transient scratch maps). The stack-cheap, per-call seed of
/// `foldhash::fast::FixedState` makes it ~10 % faster than `RandomState`
/// when keys are non-hostile (e.g. always `usize` or our own enums).
pub type FastHashMap<K, V> =
    hashbrown::HashMap<K, V, foldhash::fast::FixedState>;
