//! `LibMap`: a `std::unordered_map<std::string, V>` iteration-order
//! simulation for the libstdc++ the reference binary was built with.
//!
//! Upstream iterates `std::unordered_map` instances (the autogen parse
//! cache and the AUTOUIC per-file option map) when serializing files and
//! hashing settings, so the order leaks into observable output.  The
//! simulation reproduces libstdc++'s `_Hashtable` layout: string keys are
//! hashed with `_Hash_bytes` (MurmurHash64A seeded with `0xc70f6907`),
//! buckets grow through `_Prime_rehash_policy` (start bucket 13, load
//! factor 1.0, growth factor 2), and the global node list is maintained
//! exactly like `_M_insert_bucket_begin`: an element landing in an empty
//! bucket is prepended to the list, otherwise it is inserted at the head
//! of its bucket's run.  Verified byte-for-byte against `unordered_map`
//! iteration on this toolchain.

/// libstdc++ `_Hash_bytes` (MurmurHash64A) used by `std::hash<string>`.
fn hash_bytes(data: &[u8]) -> u64 {
    const M: u64 = 0xc6a4a7935bd1e995;
    let mut h = 0xc70f6907u64 ^ (data.len() as u64).wrapping_mul(M);
    let chunks = data.len() / 8 * 8;
    for chunk in data[..chunks].chunks_exact(8) {
        let mut k = u64::from_le_bytes(chunk.try_into().unwrap());
        k = k.wrapping_mul(M);
        k ^= k >> 47;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    for (i, &b) in data[chunks..].iter().enumerate() {
        h ^= (b as u64) << (i * 8);
    }
    if data.len() & 7 != 0 {
        h = h.wrapping_mul(M);
    }
    h ^= h >> 47;
    h = h.wrapping_mul(M);
    h ^= h >> 47;
    h
}

/// The start of libstdc++'s `__prime_list`; larger sizes fall back to a
/// prime search (the map never gets that large here).
const PRIMES: &[usize] = &[
    2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
    103, 109, 113, 127, 137, 139, 149, 157, 167, 179, 193, 199, 211, 227, 241, 257, 277, 293, 313,
    337, 359, 383, 409, 439, 467, 503, 541, 577, 619, 661, 709, 761, 823, 887, 953, 1031, 1109,
    1193, 1289, 1381, 1493, 1613, 1741, 1879, 2029, 2179, 2357, 2549, 2753, 2971, 3209, 3469, 3739,
    4027, 4349, 4703, 5087, 5503, 5953, 6427, 6949, 7517, 8123, 8783, 9497, 10273, 11113, 12011,
    12983, 14033, 15173, 16411, 17749, 19183, 20753, 22447, 24281, 26267, 28411, 30727, 33223,
    35933, 38873, 42043, 45481, 49201, 53201, 57557, 62233, 67307, 72817, 78779, 85229, 92203,
    99733,
];

fn next_bkt(n: usize) -> usize {
    if let Some(&p) = PRIMES.iter().find(|&&p| p >= n) {
        return p;
    }
    let mut candidate = n | 1;
    loop {
        if (2..)
            .take_while(|i| i * i <= candidate)
            .all(|i| !candidate.is_multiple_of(i))
        {
            return candidate;
        }
        candidate += 2;
    }
}

/// Unique-key map with libstdc++ `unordered_map` iteration order.
/// Only the operations upstream uses are provided.
pub(crate) struct LibMap<V> {
    /// The node list in iteration order (list head first).
    list: Vec<(String, V)>,
    /// Current bucket count (`_M_buckets` size).
    bucket_count: usize,
    /// `_M_next_resize` from `_Prime_rehash_policy`.
    next_resize: usize,
}

impl<V> Default for LibMap<V> {
    fn default() -> Self {
        LibMap {
            list: Vec::new(),
            bucket_count: 1,
            next_resize: 0,
        }
    }
}

impl<V> LibMap<V> {
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.list.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// `map.find(key) != map.end()`.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.list.iter().any(|(k, _)| k == key)
    }

    /// `map[key]` (insert default when absent), then mutable access.
    pub(crate) fn entry_or_default(&mut self, key: &str) -> &mut V
    where
        V: Default,
    {
        if !self.contains(key) {
            self.insert(key.to_owned(), V::default());
        }
        let index = self.list.iter().position(|(k, _)| k == key).unwrap();
        &mut self.list[index].1
    }

    /// Iteration-order access.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(String, V)> {
        self.list.iter()
    }

    /// `emplace(key, value)` for a key that is not present.
    pub(crate) fn insert(&mut self, key: String, value: V) {
        self.rehash_for(1);
        self.insert_raw(key, value);
    }

    /// `_Prime_rehash_policy::_M_need_rehash` with the default
    /// `_M_start_bkt` (13), load factor 1.0 and growth factor 2.
    fn rehash_for(&mut self, inserts: usize) {
        if self.list.len() + inserts > self.next_resize {
            let min_bkts =
                (inserts + self.list.len()).max(if self.next_resize == 0 { 13 } else { 0 });
            if min_bkts >= self.bucket_count {
                let new_count = next_bkt(min_bkts.max(2 * self.bucket_count));
                let old = std::mem::take(&mut self.list);
                self.bucket_count = new_count;
                for (key, value) in old {
                    self.insert_raw(key, value);
                }
                self.next_resize = new_count;
            } else {
                self.next_resize = self.bucket_count;
            }
        }
    }

    /// `_M_insert_bucket_begin`: prepend to the list for an empty bucket,
    /// otherwise insert at the bucket's head position.
    fn insert_raw(&mut self, key: String, value: V) {
        let bucket = (hash_bytes(key.as_bytes()) % self.bucket_count as u64) as usize;
        let head = self.list.iter().position(|(k, _)| {
            (hash_bytes(k.as_bytes()) % self.bucket_count as u64) as usize == bucket
        });
        match head {
            Some(index) => self.list.insert(index, (key, value)),
            None => self.list.insert(0, (key, value)),
        }
    }
}
