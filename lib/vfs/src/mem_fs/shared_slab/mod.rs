pub(crate) mod ropes_abuf;

use std::cell::UnsafeCell;
use std::collections::{hash_map, HashMap};
use serde::{Deserialize, Serialize};
use js_sys::ArrayBuffer;

use ropes_abuf::{Chunked, Ropes};
use crate::{Result, FsError};
use crate::mem_fs::slab_adapter::SlabAdapter;


/**
 * A `SharedSlab` uses a backing `Ropes` storage to place nodes.
 * Data is serialized and deserialized when it is modified or accessed;
 * this allows sharing the `HookedSlab` with Web Workers by building
 * `Ropes` over a JavaScript `SharedArrayBuffer`.
 */
// #[derive(Default, Clone)]  <- cannot! this will impose a constraint on T
pub struct SharedSlab<T: Serialize + for <'de> Deserialize<'de>> {
    cache: UnsafeCell<HashMap<usize, VEntry<T>>>,
    ropes: Option<UnsafeCell<Ropes>>,
    next_key: usize
}

struct VEntry<T> {
    ver: u32, val: T
}

impl<T> SharedSlab<T> where T: Serialize + for <'de> Deserialize<'de> {
    pub fn attach(&mut self, abuf: ArrayBuffer) -> Result<()> {
        t("attach");
        self.ropes = Some(UnsafeCell::new(
            Ropes::new(Chunked::new(abuf, 1024))));
        t(format!("attach: root @ ver {}", self._ropes().unwrap().ver_peek(0)).as_str());
        if self._ropes().unwrap().ver_peek(0) == 0 {
            self.push(0, &self._cache().get(&0).unwrap().val)?;
        }
        Ok(())
    }

    fn same_ver(e: Option<&VEntry<T>>, ver: u32) -> bool {
        e.map(|e| e.ver == ver).unwrap_or(false)
    }

    fn pull(&self, key: usize) -> Result<Option<&mut T>> {
        //t(format!("pull({key})").as_str());
        if let Some(ropes) = self._ropes() {
            let ver = ropes.ver_peek(key);
            if !SharedSlab::same_ver(self._cache().get(&key), ver) {
                t(format!("pull({key}): cache miss").as_str());
                let data = ropes.get(key);
                let bytes = &data[..];
                let val = serde_cbor::from_slice(bytes).map_err(|_| FsError::InvalidData)?;
                self._cache().insert(key, VEntry { ver, val });  // @oops this is now owned by the cache
            }
        }
        Ok(self._cache().get_mut(&key).map(|e| &mut e.val))
    }

    fn push(&self, key: usize, val: &T) -> Result<u32> {
        t(format!("push({key}, ..)").as_str());
        let s = serde_cbor::ser::to_vec(val).map_err(|_| FsError::IOError)?;
        t(format!("    [{}]", s.len()).as_str());
        let bytes = &s[..]; //s.as_bytes();
        if let Some(ropes) = self._ropes() {
            Ok(ropes.insert_at(key, bytes))
        }
        else { Ok(0) }
    }

    fn _cache(&self) -> &mut HashMap<usize, VEntry<T>> { unsafe { &mut *self.cache.get() } }
    fn _ropes(&self) -> Option<&mut Ropes> {
        self.ropes.as_ref().map(|e| unsafe { &mut *e.get() })
    }
}

struct Iter<'a, T> { entries: hash_map::Iter<'a, usize, VEntry<T>> }

impl<'a, T> Iter<'a, T> {
    fn new(entries: hash_map::Iter<'a, usize, VEntry<T>>) -> Self { Iter { entries } }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = (usize, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|e| (*e.0, &e.1.val))
    }
}

unsafe impl<T: Serialize + for <'de> Deserialize<'de>> Send for SharedSlab<T> { }
unsafe impl<T: Serialize + for <'de> Deserialize<'de>> Sync for SharedSlab<T> { }

fn t(s: &str) {
    web_sys::console::log_2(&"[HookedSlab]".into(), &s.into());
}

impl<T: Serialize + for <'de> Deserialize<'de>> SharedSlab<T> {

    pub fn new() -> Self {
        t("created");
        SharedSlab {
            cache: UnsafeCell::new(HashMap::new()),
            ropes: None, next_key: 0
        }
    }

    pub fn get(&self, key: usize) -> Option<&T> {
        //t(format!("get({key})").as_str());
        self.pull(key).unwrap().map(|t| t as &T)
    }
    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        t(format!("get_mut({key})").as_str());
        self.pull(key).unwrap()
    }

    /**
     * Writes updated values to the storage.
     * This function is not `&mut self` for technical reasons.
     */
    pub fn flush_mut(&self, key: usize) {
        let e = self._cache().get_mut(&key).unwrap();
        e.ver = self.push(key, &e.val).unwrap();
    }

    fn alloc(&mut self) -> usize {
        /* @todo reuse deallocated elements! */
        let key = self._ropes().map(|e| e.alloc()).unwrap_or(self.next_key);
        self.next_key = key + 1;
        key
    }

    fn alloc_peek(&mut self) -> usize {
        self._ropes().map(|e| e.alloc_peek()).unwrap_or(self.next_key)
    }

    pub fn insert(&mut self, val: T) -> usize {
        let key = self.alloc();
        t(format!("insert {key}").as_str());
        self._cache().insert(key, VEntry { ver: 0, val });
        self.flush_mut(key);
        key
    }
    pub fn remove(&mut self, key: usize) -> T {
        t(format!("remove{key}").as_str());
        self._ropes().unwrap().remove(key);
        self._cache().remove(&key).unwrap().val
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = (usize, &'_ T)> + '_> {
        t("iter");
        // This only iterates cached entries. Is this ok?
        // (`iter()` is only called from fs `unlink`, where it is used to find
        //  the directory containing a file being removed so it can be updated)
        Box::new(Iter::new(self._cache().iter()))
    }
}

impl<T> Default for SharedSlab<T> where T: Serialize + for <'de> Deserialize<'de> {
    fn default() -> Self { SharedSlab::new() }
}

impl<T> SlabAdapter<T> for SharedSlab<T> where T: 'static + Serialize + for <'de> Deserialize<'de> {
    fn new() -> Self { SharedSlab::new() }

    fn get(&self, key: usize) -> Option<&T> { SharedSlab::get(self, key) }
    fn get_mut(&mut self, key: usize) -> Option<&mut T> { SharedSlab::get_mut(self, key) }

    fn flush_mut(&self, key: usize) { SharedSlab::flush_mut(self, key) }

    fn insert(&mut self, val: T) -> usize { SharedSlab::insert(self, val) }
    fn remove(&mut self, key: usize) -> T { SharedSlab::remove(self, key) }

    fn iter(&self) -> Box<dyn Iterator<Item = (usize, &'_ T)> + '_> { SharedSlab::iter(self) }
    fn vacant_entry_key(&mut self) -> usize { self.alloc_peek() }
}
