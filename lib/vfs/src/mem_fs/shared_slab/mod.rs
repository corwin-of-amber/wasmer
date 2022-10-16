pub(crate) mod ui8a_ropes;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use js_sys::Uint8Array;

use ui8a_ropes::{Chunked, Ropes};
use crate::{Result, FsError};


// #[derive(Clone)]
pub struct HookedSlab<T: Serialize + for <'de> Deserialize<'de>> {
    cache: UnsafeCell<HashMap<usize, VEntry<T>>>,
    ropes: Option<UnsafeCell<Ropes>>,
    next_key: usize
}

struct VEntry<T> {
    ver: u32, val: T
}

pub struct VacantEntry {
    key: usize
}

impl VacantEntry { pub fn key(&self) -> usize { self.key } }

impl<T> HookedSlab<T> where T: Serialize + for <'de> Deserialize<'de> {
    pub fn attach(&mut self, ui8a: Uint8Array) -> Result<()> {
        t("attach");
        self.ropes = Some(UnsafeCell::new(
            Ropes::new(Chunked::new(ui8a.buffer(), 1024))));
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
            if !HookedSlab::same_ver(self._cache().get(&key), ver) {
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

unsafe impl<T: Serialize + for <'de> Deserialize<'de>> Send for HookedSlab<T> { }
unsafe impl<T: Serialize + for <'de> Deserialize<'de>> Sync for HookedSlab<T> { }

fn t(s: &str) {
    web_sys::console::log_2(&"[HookedSlab]".into(), &s.into());
}

impl<T: Serialize + for <'de> Deserialize<'de>> HookedSlab<T> {

    pub fn new() -> Self {
        t("created");
        HookedSlab {
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
    pub fn remove(&mut self, key: usize) -> T { t("remove"); todo!() }

    pub fn iter(&self) -> slab::Iter<'_, T> { t("iter"); todo!() }
    pub fn vacant_entry(&mut self) -> VacantEntry { VacantEntry { key: self.alloc_peek() } }
}
