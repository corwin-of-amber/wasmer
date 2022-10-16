use slab::Slab;

pub trait SlabAdapter<T: 'static> : 'static + Default + Send + Sync {
    fn new() -> Self;

    fn get(&self, key: usize) -> Option<&T>;
    fn get_mut(&mut self, key: usize) -> Option<&mut T>;

    fn flush_mut(&self, key: usize);

    fn insert(&mut self, val: T) -> usize;
    fn remove(&mut self, key: usize) -> T;

    fn iter(&self) -> Box<dyn Iterator<Item = (usize, &'_ T)> + '_>;
    fn vacant_entry_key(&mut self) -> usize;
}

impl<T> SlabAdapter<T> for Slab<T> where T: 'static + Send + Sync {
    fn new() -> Self { Slab::new() }

    fn get(&self, key: usize) -> Option<&T> { Slab::get(self, key) }
    fn get_mut(&mut self, key: usize) -> Option<&mut T> { Slab::get_mut(self, key) }

    fn flush_mut(&self, _key: usize) { }

    fn insert(&mut self, val: T) -> usize { Slab::insert(self, val) }
    fn remove(&mut self, key: usize) -> T { Slab::remove(self, key) }

    fn iter(&self) -> Box<dyn Iterator<Item = (usize, &'_ T)> + '_> { Box::new(Slab::iter(self)) }
    fn vacant_entry_key(&mut self) -> usize { self.vacant_entry().key() }
}
