use std::sync::{Arc, RwLock, LazyLock};
use lru::LruCache;

use wasmer_types::{
    ExternType, ModuleHash, ModuleInfo,
}; 

/// WebAssembly in the browser doesn't yet output the descriptor/types
/// corresponding to each extern (import and export).
///
/// This should be fixed once the JS-Types Wasm proposal is adopted
/// by the browsers:
/// <https://github.com/WebAssembly/js-types/blob/master/proposals/js-types/Overview.md>
///
/// Until that happens, we annotate the module with the expected
/// types so we can built on top of them at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleTypeHints {
    /// The type hints for the imported types
    pub imports: Vec<ExternType>,
    /// The type hints for the exported types
    pub exports: Vec<ExternType>,
}

type Info = (Option<Arc<ModuleTypeHints>>, Option<String>);
type InfoRef<'a> = (&'a Option<Arc<ModuleTypeHints>>, &'a Option<String>);

static G_CACHE: LazyLock<RwLock<LruCache<ModuleHash, Info>>> = 
    LazyLock::new(|| RwLock::new(LruCache::unbounded()));

static G_CACHE_MINSZ: usize = 30_000_000;  // don't bother with smaller modules

pub fn get(key: &ModuleHash) -> Option<Info> {
    G_CACHE.write().ok().and_then(|mut g| g.get(&key).map(|o| o.clone()))
}

pub fn put<'a>(key: &ModuleHash, sz: usize, info: InfoRef<'a>) {
    if sz > G_CACHE_MINSZ && info.1.is_some() {
        if let Ok(mut g) = G_CACHE.write() {
            g.put(*key, (info.0.clone(), info.1.clone()));
        }
    }
}