#![allow(unsafe_code)]
//! This is the core implementation that doesn't depend on the hasher at all.
//!
//! The methods of `IndexMapCore` don't use any Hash properties of K.
//!
//! It's cleaner to separate them out, then the compiler checks that we are not
//! using Hash at all in these methods.
//!
//! However, we should probably not let this show in the public API or docs.

#[cfg(not(has_std))]
use std::vec::Vec;

use hashbrown::raw::Bucket as RawBucket;
use hashbrown::raw::RawTable;

use std::cmp;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::mem::replace;
use std::ops::RangeFull;
use std::vec::Drain;

use equivalent::Equivalent;
use util::enumerate;
use {Bucket, Entries, HashValue};

/// Core of the map that does not depend on S
pub(crate) struct IndexMapCore<K, V, Idx = usize> {
    /// indices mapping from the entry hash to its index.
    indices: RawTable<Idx>,
    /// entries is a dense vec of entries in their order.
    entries: Vec<Bucket<K, V>>,
}

pub trait Index: Copy + Send + Sync + Ord + TryFrom<usize> + TryInto<usize> {
    /// Creates an index from a `usize`; panics on failure.
    fn from_usize(i: usize) -> Self {
        match Self::try_from(i) {
            Ok(i) => i,
            Err(_) => panic!("invalid index!"),
        }
    }

    /// Converts the index to a `usize`; panics on failure.
    fn into_usize(self) -> usize {
        match self.try_into() {
            Ok(i) => i,
            Err(_) => panic!("invalid index!"),
        }
    }
}

impl<T> Index for T where T: Copy + Send + Sync + Ord + TryFrom<usize> + TryInto<usize> {}

#[inline(always)]
fn get_hash<K, V, Idx: Index>(entries: &[Bucket<K, V>]) -> impl Fn(&Idx) -> u64 + '_ {
    move |&i| entries[i.into_usize()].hash.get()
}

impl<K, V, Idx> Clone for IndexMapCore<K, V, Idx>
where
    K: Clone,
    V: Clone,
    Idx: Index,
{
    fn clone(&self) -> Self {
        let indices = self.indices.clone();
        let mut entries = Vec::with_capacity(indices.capacity());
        entries.clone_from(&self.entries);
        IndexMapCore { indices, entries }
    }

    fn clone_from(&mut self, other: &Self) {
        let hasher = get_hash(&other.entries);
        self.indices.clone_from_with_hasher(&other.indices, hasher);
        if self.entries.capacity() < other.entries.len() {
            // If we must resize, match the indices capacity
            self.reserve_entries();
        }
        self.entries.clone_from(&other.entries);
    }
}

impl<K, V, Idx> fmt::Debug for IndexMapCore<K, V, Idx>
where
    K: fmt::Debug,
    V: fmt::Debug,
    Idx: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct DebugIndices<'a, Idx>(&'a RawTable<Idx>);
        impl<Idx: fmt::Debug> fmt::Debug for DebugIndices<'_, Idx> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let indices = unsafe { self.0.iter().map(|raw_bucket| raw_bucket.read()) };
                f.debug_list().entries(indices).finish()
            }
        }

        f.debug_struct("IndexMapCore")
            .field("indices", &DebugIndices(&self.indices))
            .field("entries", &self.entries)
            .finish()
    }
}

impl<K, V, Idx: Index> Entries for IndexMapCore<K, V, Idx> {
    type Entry = Bucket<K, V>;

    #[inline]
    fn into_entries(self) -> Vec<Self::Entry> {
        self.entries
    }

    #[inline]
    fn as_entries(&self) -> &[Self::Entry] {
        &self.entries
    }

    #[inline]
    fn as_entries_mut(&mut self) -> &mut [Self::Entry] {
        &mut self.entries
    }

    fn with_entries<F>(&mut self, f: F)
    where
        F: FnOnce(&mut [Self::Entry]),
    {
        f(&mut self.entries);
        self.rebuild_hash_table();
    }
}

impl<K, V, Idx: Index> IndexMapCore<K, V, Idx> {
    #[inline]
    pub(crate) fn new() -> Self {
        IndexMapCore {
            indices: RawTable::new(),
            entries: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn with_capacity(n: usize) -> Self {
        IndexMapCore {
            indices: RawTable::with_capacity(n),
            entries: Vec::with_capacity(n),
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.indices.len()
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        cmp::min(self.indices.capacity(), self.entries.capacity())
    }

    pub(crate) fn clear(&mut self) {
        self.indices.clear_no_drop();
        self.entries.clear();
    }

    pub(crate) fn drain(&mut self, range: RangeFull) -> Drain<'_, Bucket<K, V>> {
        self.indices.clear_no_drop();
        self.entries.drain(range)
    }

    /// Reserve capacity for `additional` more key-value pairs.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.indices.reserve(additional, get_hash(&self.entries));
        self.reserve_entries();
    }

    /// Reserve entries capacity to match the indices
    fn reserve_entries(&mut self) {
        let additional = self.indices.capacity() - self.entries.len();
        self.entries.reserve_exact(additional);
    }

    /// Shrink the capacity of the map as much as possible.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.indices.shrink_to(0, get_hash(&self.entries));
        self.entries.shrink_to_fit();
    }

    /// Remove the last key-value pair
    pub(crate) fn pop(&mut self) -> Option<(K, V)> {
        if let Some(entry) = self.entries.pop() {
            let last = Idx::from_usize(self.entries.len());
            let raw_bucket = self.find_index(entry.hash, last).unwrap();
            unsafe { self.indices.erase_no_drop(&raw_bucket) };
            Some((entry.key, entry.value))
        } else {
            None
        }
    }

    /// Append a key-value pair, *without* checking whether it already exists,
    /// and return the pair's new index.
    fn push(&mut self, hash: HashValue, key: K, value: V) -> Idx {
        let len = self.entries.len();
        let i = Idx::from_usize(len);
        self.indices.insert(hash.get(), i, get_hash(&self.entries));
        if len == self.entries.capacity() {
            // Reserve our own capacity synced to the indices,
            // rather than letting `Vec::push` just double it.
            self.reserve_entries();
        }
        self.entries.push(Bucket { hash, key, value });
        i
    }

    /// Return the index in `entries` where an equivalent key can be found
    pub(crate) fn get_index_of<Q>(&self, hash: HashValue, key: &Q) -> Option<Idx>
    where
        Q: ?Sized + Equivalent<K>,
    {
        match self.find_equivalent(hash, key) {
            Some(raw_bucket) => Some(unsafe { raw_bucket.read() }),
            None => None,
        }
    }

    pub(crate) fn insert_full(&mut self, hash: HashValue, key: K, value: V) -> (Idx, Option<V>)
    where
        K: Eq,
    {
        match self.get_index_of(hash, &key) {
            Some(i) => (
                i,
                Some(replace(&mut self.entries[i.into_usize()].value, value)),
            ),
            None => (self.push(hash, key, value), None),
        }
    }

    pub(crate) fn entry(&mut self, hash: HashValue, key: K) -> Entry<'_, K, V, Idx>
    where
        K: Eq,
    {
        match self.find_equivalent(hash, &key) {
            Some(raw_bucket) => Entry::Occupied(OccupiedEntry {
                map: self,
                raw_bucket,
                key,
            }),
            None => Entry::Vacant(VacantEntry {
                map: self,
                hash,
                key,
            }),
        }
    }

    /// Return the raw bucket with an equivalent key
    fn find_equivalent<Q>(&self, hash: HashValue, key: &Q) -> Option<RawBucket<Idx>>
    where
        Q: ?Sized + Equivalent<K>,
    {
        self.indices.find(hash.get(), {
            |&i| Q::equivalent(key, &self.entries[i.into_usize()].key)
        })
    }

    /// Return the raw bucket for the given index
    fn find_index(&self, hash: HashValue, index: Idx) -> Option<RawBucket<Idx>> {
        self.indices.find(hash.get(), |&i| i == index)
    }

    /// Remove an entry by shifting all entries that follow it
    pub(crate) fn shift_remove_full<Q>(&mut self, hash: HashValue, key: &Q) -> Option<(Idx, K, V)>
    where
        Q: ?Sized + Equivalent<K>,
    {
        match self.find_equivalent(hash, key) {
            Some(raw_bucket) => Some(self.shift_remove_bucket(raw_bucket)),
            None => None,
        }
    }

    /// Remove an entry by shifting all entries that follow it
    pub(crate) fn shift_remove_index(&mut self, index: Idx) -> Option<(K, V)> {
        let raw_bucket = match self.entries.get(index.into_usize()) {
            Some(entry) => self.find_index(entry.hash, index).unwrap(),
            None => return None,
        };
        let (_, key, value) = self.shift_remove_bucket(raw_bucket);
        Some((key, value))
    }

    /// Remove an entry by shifting all entries that follow it
    fn shift_remove_bucket(&mut self, raw_bucket: RawBucket<Idx>) -> (Idx, K, V) {
        // use Vec::remove, but then we need to update the indices that point
        // to all of the other entries that have to move
        let index = unsafe {
            self.indices.erase_no_drop(&raw_bucket);
            raw_bucket.read()
        };
        let index_usize = index.into_usize();
        let entry = self.entries.remove(index_usize);

        // correct indices that point to the entries that followed the removed entry.
        // use a heuristic between a full sweep vs. a `find()` for every shifted item.
        let raw_capacity = self.indices.buckets();
        let shifted_entries = &self.entries[index_usize..];
        if shifted_entries.len() > raw_capacity / 2 {
            // shift all indices greater than `index`
            unsafe {
                for bucket in self.indices.iter() {
                    let i = bucket.read();
                    if i > index {
                        bucket.write(Idx::from_usize(i.into_usize() - 1));
                    }
                }
            }
        } else {
            // find each following entry to shift its index
            for (i, entry) in (index_usize + 1..).zip(shifted_entries) {
                let i = Idx::from_usize(i);
                let shifted_bucket = self.find_index(entry.hash, i).unwrap();
                unsafe { shifted_bucket.write(Idx::from_usize(i.into_usize() - 1)) };
            }
        }

        (index, entry.key, entry.value)
    }

    /// Remove an entry by swapping it with the last
    pub(crate) fn swap_remove_full<Q>(&mut self, hash: HashValue, key: &Q) -> Option<(Idx, K, V)>
    where
        Q: ?Sized + Equivalent<K>,
    {
        match self.find_equivalent(hash, key) {
            Some(raw_bucket) => Some(self.swap_remove_bucket(raw_bucket)),
            None => None,
        }
    }

    /// Remove an entry by swapping it with the last
    pub(crate) fn swap_remove_index(&mut self, index: Idx) -> Option<(K, V)> {
        let raw_bucket = match self.entries.get(index.into_usize()) {
            Some(entry) => self.find_index(entry.hash, index).unwrap(),
            None => return None,
        };
        let (_, key, value) = self.swap_remove_bucket(raw_bucket);
        Some((key, value))
    }

    /// Remove an entry by swapping it with the last
    fn swap_remove_bucket(&mut self, raw_bucket: RawBucket<Idx>) -> (Idx, K, V) {
        // use swap_remove, but then we need to update the index that points
        // to the other entry that has to move
        let index = unsafe {
            self.indices.erase_no_drop(&raw_bucket);
            raw_bucket.read()
        };
        let index_usize = index.into_usize();
        let entry = self.entries.swap_remove(index_usize);

        // correct index that points to the entry that had to swap places
        if let Some(entry) = self.entries.get(index_usize) {
            // was not last element
            // examine new element in `index` and find it in indices
            let last = Idx::from_usize(self.entries.len());
            let swapped_bucket = self.find_index(entry.hash, last).unwrap();
            unsafe { swapped_bucket.write(index) };
        }

        (index, entry.key, entry.value)
    }

    pub(crate) fn retain_in_order<F>(&mut self, mut keep: F)
    where
        F: FnMut(&mut K, &mut V) -> bool,
    {
        // Like Vec::retain in self.entries, but with mutable K and V.
        // We swap-shift all the items we want to keep, truncate the rest,
        // then rebuild the raw hash table with the new indexes.
        let len = self.entries.len();
        let mut n_deleted = 0;
        for i in 0..len {
            let will_keep = {
                let entry = &mut self.entries[i];
                keep(&mut entry.key, &mut entry.value)
            };
            if !will_keep {
                n_deleted += 1;
            } else if n_deleted > 0 {
                self.entries.swap(i - n_deleted, i);
            }
        }
        if n_deleted > 0 {
            self.entries.truncate(len - n_deleted);
            self.rebuild_hash_table();
        }
    }

    pub(crate) fn reverse(&mut self) {
        self.entries.reverse();

        // No need to save hash indices, can easily calculate what they should
        // be, given that this is an in-place reversal.
        let len = self.entries.len();
        unsafe {
            for raw_bucket in self.indices.iter() {
                let i = raw_bucket.read();
                let j = len - i.into_usize() - 1;
                raw_bucket.write(Idx::from_usize(j));
            }
        }
    }

    fn rebuild_hash_table(&mut self) {
        self.indices.clear_no_drop();
        debug_assert!(self.indices.capacity() >= self.entries.len());
        for (i, entry) in enumerate(&self.entries) {
            let i = Idx::from_usize(i);
            // We should never have to reallocate, so there's no need for a real hasher.
            self.indices.insert(entry.hash.get(), i, |_| unreachable!());
        }
    }
}

/// Entry for an existing key-value pair or a vacant location to
/// insert one.
pub enum Entry<'a, K, V, Idx = usize> {
    /// Existing slot with equivalent key.
    Occupied(OccupiedEntry<'a, K, V, Idx>),
    /// Vacant slot (no equivalent key in the map).
    Vacant(VacantEntry<'a, K, V, Idx>),
}

impl<'a, K, V, Idx: Index> Entry<'a, K, V, Idx> {
    /// Computes in **O(1)** time (amortized average).
    pub fn or_insert(self, default: V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(default),
        }
    }

    /// Computes in **O(1)** time (amortized average).
    pub fn or_insert_with<F>(self, call: F) -> &'a mut V
    where
        F: FnOnce() -> V,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(call()),
        }
    }

    pub fn key(&self) -> &K {
        match *self {
            Entry::Occupied(ref entry) => entry.key(),
            Entry::Vacant(ref entry) => entry.key(),
        }
    }

    /// Return the index where the key-value pair exists or will be inserted.
    pub fn index(&self) -> Idx {
        match *self {
            Entry::Occupied(ref entry) => entry.index(),
            Entry::Vacant(ref entry) => entry.index(),
        }
    }

    /// Modifies the entry if it is occupied.
    pub fn and_modify<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut V),
    {
        match self {
            Entry::Occupied(mut o) => {
                f(o.get_mut());
                Entry::Occupied(o)
            }
            x => x,
        }
    }

    /// Inserts a default-constructed value in the entry if it is vacant and returns a mutable
    /// reference to it. Otherwise a mutable reference to an already existent value is returned.
    ///
    /// Computes in **O(1)** time (amortized average).
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(V::default()),
        }
    }
}

impl<'a, K: 'a + fmt::Debug, V: 'a + fmt::Debug, Idx: Index> fmt::Debug for Entry<'a, K, V, Idx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Entry::Vacant(ref v) => f.debug_tuple(stringify!(Entry)).field(v).finish(),
            Entry::Occupied(ref o) => f.debug_tuple(stringify!(Entry)).field(o).finish(),
        }
    }
}

/// A view into an occupied entry in a `IndexMap`.
/// It is part of the [`Entry`] enum.
///
/// [`Entry`]: enum.Entry.html
pub struct OccupiedEntry<'a, K, V, Idx> {
    map: &'a mut IndexMapCore<K, V, Idx>,
    raw_bucket: RawBucket<Idx>,
    key: K,
}

// `hashbrown::raw::Bucket` is only `Send`, not `Sync`.
// SAFETY: `&self` only accesses the bucket to read it.
unsafe impl<K: Sync, V: Sync, Idx: Sync> Sync for OccupiedEntry<'_, K, V, Idx> {}

impl<'a, K, V, Idx: Index> OccupiedEntry<'a, K, V, Idx> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn get(&self) -> &V {
        &self.map.entries[self.index_usize()].value
    }

    pub fn get_mut(&mut self) -> &mut V {
        let index = self.index_usize();
        &mut self.map.entries[index].value
    }

    /// Put the new key in the occupied entry's key slot
    pub(crate) fn replace_key(self) -> K {
        let index = self.index_usize();
        let old_key = &mut self.map.entries[index].key;
        replace(old_key, self.key)
    }

    /// Return the index of the key-value pair
    #[inline]
    pub fn index(&self) -> Idx {
        unsafe { self.raw_bucket.read() }
    }

    #[inline]
    fn index_usize(&self) -> usize {
        self.index().into_usize()
    }

    pub fn into_mut(self) -> &'a mut V {
        let index = self.index_usize();
        &mut self.map.entries[index].value
    }

    /// Sets the value of the entry to `value`, and returns the entry's old value.
    pub fn insert(&mut self, value: V) -> V {
        replace(self.get_mut(), value)
    }

    /// Remove the key, value pair stored in the map for this entry, and return the value.
    ///
    /// **NOTE:** This is equivalent to `.swap_remove()`.
    pub fn remove(self) -> V {
        self.swap_remove()
    }

    /// Remove the key, value pair stored in the map for this entry, and return the value.
    ///
    /// Like `Vec::swap_remove`, the pair is removed by swapping it with the
    /// last element of the map and popping it off. **This perturbs
    /// the postion of what used to be the last element!**
    ///
    /// Computes in **O(1)** time (average).
    pub fn swap_remove(self) -> V {
        self.swap_remove_entry().1
    }

    /// Remove the key, value pair stored in the map for this entry, and return the value.
    ///
    /// Like `Vec::remove`, the pair is removed by shifting all of the
    /// elements that follow it, preserving their relative order.
    /// **This perturbs the index of all of those elements!**
    ///
    /// Computes in **O(n)** time (average).
    pub fn shift_remove(self) -> V {
        self.shift_remove_entry().1
    }

    /// Remove and return the key, value pair stored in the map for this entry
    ///
    /// **NOTE:** This is equivalent to `.swap_remove_entry()`.
    pub fn remove_entry(self) -> (K, V) {
        self.swap_remove_entry()
    }

    /// Remove and return the key, value pair stored in the map for this entry
    ///
    /// Like `Vec::swap_remove`, the pair is removed by swapping it with the
    /// last element of the map and popping it off. **This perturbs
    /// the postion of what used to be the last element!**
    ///
    /// Computes in **O(1)** time (average).
    pub fn swap_remove_entry(self) -> (K, V) {
        let (_, key, value) = self.map.swap_remove_bucket(self.raw_bucket);
        (key, value)
    }

    /// Remove and return the key, value pair stored in the map for this entry
    ///
    /// Like `Vec::remove`, the pair is removed by shifting all of the
    /// elements that follow it, preserving their relative order.
    /// **This perturbs the index of all of those elements!**
    ///
    /// Computes in **O(n)** time (average).
    pub fn shift_remove_entry(self) -> (K, V) {
        let (_, key, value) = self.map.shift_remove_bucket(self.raw_bucket);
        (key, value)
    }
}

impl<'a, K: 'a + fmt::Debug, V: 'a + fmt::Debug, Idx: Index> fmt::Debug
    for OccupiedEntry<'a, K, V, Idx>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(stringify!(OccupiedEntry))
            .field("key", self.key())
            .field("value", self.get())
            .finish()
    }
}

/// A view into a vacant entry in a `IndexMap`.
/// It is part of the [`Entry`] enum.
///
/// [`Entry`]: enum.Entry.html
pub struct VacantEntry<'a, K, V, Idx> {
    map: &'a mut IndexMapCore<K, V, Idx>,
    hash: HashValue,
    key: K,
}

impl<'a, K, V, Idx: Index> VacantEntry<'a, K, V, Idx> {
    pub fn key(&self) -> &K {
        &self.key
    }

    pub fn into_key(self) -> K {
        self.key
    }

    /// Return the index where the key-value pair will be inserted.
    pub fn index(&self) -> Idx {
        Idx::from_usize(self.map.len())
    }

    pub fn insert(self, value: V) -> &'a mut V {
        let i = self.map.push(self.hash, self.key, value);
        &mut self.map.entries[i.into_usize()].value
    }
}

impl<'a, K: 'a + fmt::Debug, V: 'a, Idx: Index> fmt::Debug for VacantEntry<'a, K, V, Idx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(stringify!(VacantEntry))
            .field(self.key())
            .finish()
    }
}

#[test]
fn assert_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<IndexMapCore<i32, i32>>();
    assert_send_sync::<Entry<i32, i32>>();
}
