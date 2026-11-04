use std::{
    fmt::{self, Display, Formatter},
    hash::Hash,
};

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
#[cfg(feature = "enable-serde")]
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// The hash of a WebAssembly module.
#[derive(
    Debug,
    Copy,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    RkyvSerialize,
    RkyvDeserialize,
    Archive,
)]
#[rkyv(derive(Debug))]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub struct ModuleHash([u8; 32]);

#[cfg(feature = "artifact-size")]
impl loupe::MemoryUsage for ModuleHash {
    fn size_of_val(&self, _tracker: &mut dyn loupe::MemoryUsageTracker) -> usize {
        size_of::<ModuleHash>()
    }
}

impl ModuleHash {
    /// Generate a new [`ModuleHash`] based on the Sha256 hash of some bytes.
    pub fn new(wasm: impl AsRef<[u8]>) -> Self {
        let wasm = wasm.as_ref();
        let hash = sha2::Sha256::digest(wasm).into();
        Self(hash)
    }

    /// Generate a new [`ModuleHash`] based on the Sha256 hash of some bytes.
    pub fn sha256(wasm: impl AsRef<[u8]>) -> Self {
        Self::new(wasm)
    }

    /// Create a new [`ModuleHash`] from the raw sha256 hash.
    pub fn from_bytes(hash: [u8; 32]) -> Self {
        Self(hash)
    }

    /// Generate a random [`ModuleHash`]. For when you don't care about caches.
    pub fn random() -> Self {
        let mut bytes = [0_u8; _];
        getrandom::fill(&mut bytes).unwrap();
        Self(bytes)
    }

    /// Get the raw hash.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Build a short hex representation of the hash (first 4 bytes).
    pub fn short_hash(&self) -> String {
        hex::encode_upper(&self.as_bytes()[..4])
    }
}

impl Display for ModuleHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode_upper(self.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_empty_input() {
        let expected = sha2::Sha256::digest([]).into();
        assert_eq!(ModuleHash::new([]), ModuleHash::from_bytes(expected));
    }

    #[test]
    fn hash_minimal_wasm_module() {
        let wasm = b"\0asm\x01\0\0\0";
        let expected = sha2::Sha256::digest(wasm).into();
        assert_eq!(ModuleHash::new(wasm), ModuleHash::from_bytes(expected));
    }

    #[test]
    fn hash_entire_input_around_64_kib() {
        for size in [(1 << 16) - 1, (1 << 16), (1 << 16) + 1] {
            let wasm = vec![0x42; size];
            let expected = sha2::Sha256::digest(&wasm).into();
            assert_eq!(ModuleHash::new(&wasm), ModuleHash::from_bytes(expected));
        }
    }

    #[test]
    fn hash_includes_bytes_after_64_kib() {
        let prefix = vec![0x42; 1 << 16];
        let mut longer = prefix.clone();
        longer.push(0x99);
        assert_ne!(ModuleHash::new(&prefix), ModuleHash::new(&longer));
        let mut changed = longer.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert_ne!(ModuleHash::new(&longer), ModuleHash::new(&changed));
    }
}
