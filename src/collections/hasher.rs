use std::hash::{Hash, Hasher};

// --- FNV-1a 64-bit
struct FastHasher(u64);

impl Default for FastHasher {
    #[inline]
    fn default() -> Self {
        FastHasher(0xcbf29ce484222325u64) // FNV offset basis
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // FNV-1a
        let mut hash = self.0;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3u64);
        }
        self.0 = hash;
    }
}

#[inline]
fn fast_hash<Q: ?Sized + Hash>(key: &Q) -> u64 {
    let mut h = FastHasher::default();
    key.hash(&mut h);
    h.finish()
}