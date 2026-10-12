//! Maglev consistent hashing (Eisenbud et al., NSDI 2016), with Envoy's
//! weighted extension.
//!
//! Each host gets a pseudo-random permutation of the table's slots, derived
//! from its key. Hosts take turns claiming their next free preferred slot, a
//! host of weight `w` taking a turn `w / max_weight` as often as the heaviest
//! host, until the table is full. A request hashes to a slot and goes to the
//! slot's host.
//!
//! Properties this gives (checked by the tests): every host owns a share of
//! slots proportional to its weight, and removing one host only moves the
//! slots that host owned plus a small fraction of others.

use twox_hash::XxHash64;

fn xxh64(data: &[u8], seed: u64) -> u64 {
    XxHash64::oneshot(seed, data)
}

/// Table size: a prime, as Maglev requires, and Envoy's default.
pub const TABLE_SIZE: usize = 65_537;

const OFFSET_SEED: u64 = 0x6d61_676c_6576_3031; // "maglev01"
const SKIP_SEED: u64 = 0x6d61_676c_6576_3032; // "maglev02"
/// Seed for hashing request keys, shared so every replica maps a key alike.
pub const KEY_SEED: u64 = 0x6665_7272_795f_6b31; // "ferry_k1"

/// Hashes a request key (header value, cookie value or IP) for [`Maglev::lookup`].
#[must_use]
pub fn hash_key(key: &[u8]) -> u64 {
    xxh64(key, KEY_SEED)
}

/// A built lookup table mapping hash slots to host indices.
#[derive(Clone)]
pub struct Maglev {
    table: Box<[u32]>,
}

impl std::fmt::Debug for Maglev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maglev")
            .field("size", &self.table.len())
            .finish_non_exhaustive()
    }
}

impl Maglev {
    /// Builds a table over hosts identified by `keys` (stable across replicas,
    /// such as the address text) with the given weights. Returns `None` if
    /// there are no hosts or every weight is zero.
    ///
    /// The value stored in each slot is the host's index in `keys`.
    #[must_use]
    pub fn build(keys: &[&[u8]], weights: &[u32]) -> Option<Self> {
        Self::build_with_size(keys, weights, TABLE_SIZE)
    }

    fn build_with_size(keys: &[&[u8]], weights: &[u32], size: usize) -> Option<Self> {
        let max_weight = u64::from(weights.iter().copied().max()?);
        if max_weight == 0 || keys.len() != weights.len() || size < 2 {
            return None;
        }
        let m = size as u64;
        struct Entry {
            host: u32,
            offset: u64,
            skip: u64,
            next: u64,
            weight: u64,
            target: u64,
        }
        let mut entries: Vec<Entry> = keys
            .iter()
            .zip(weights)
            .enumerate()
            .filter(|(_, (_, w))| **w > 0)
            .filter_map(|(i, (key, w))| {
                Some(Entry {
                    host: u32::try_from(i).ok()?,
                    offset: xxh64(key, OFFSET_SEED) % m,
                    skip: xxh64(key, SKIP_SEED) % (m - 1) + 1,
                    next: 0,
                    weight: u64::from(*w),
                    target: 0,
                })
            })
            .collect();
        let mut table = vec![u32::MAX; size];
        let mut filled = 0usize;
        let mut iteration: u64 = 1;
        while filled < size {
            for e in &mut entries {
                // A host of weight w claims a slot on iterations where
                // iteration * w has caught up with its running target.
                if iteration * e.weight < e.target {
                    continue;
                }
                e.target += max_weight;
                let mut slot = (e.offset + e.skip * e.next) % m;
                while table.get(slot as usize).is_some_and(|&h| h != u32::MAX) {
                    e.next += 1;
                    slot = (e.offset + e.skip * e.next) % m;
                }
                if let Some(s) = table.get_mut(slot as usize) {
                    *s = e.host;
                }
                e.next += 1;
                filled += 1;
                if filled == size {
                    break;
                }
            }
            iteration += 1;
        }
        Some(Self {
            table: table.into_boxed_slice(),
        })
    }

    /// The host index owning `hash`.
    #[must_use]
    pub fn lookup(&self, hash: u64) -> Option<usize> {
        let len = self.table.len() as u64;
        let slot = usize::try_from(hash % len).ok()?;
        self.table
            .get(slot)
            .filter(|&&h| h != u32::MAX)
            .map(|&h| h as usize)
    }

    /// Rewrites every slot's host index through `f`, for callers that built
    /// the table over a subset of their hosts.
    #[must_use]
    pub fn map_hosts(mut self, f: impl Fn(usize) -> Option<usize>) -> Self {
        for slot in &mut self.table {
            *slot = f(*slot as usize)
                .and_then(|h| u32::try_from(h).ok())
                .unwrap_or(u32::MAX);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn keys(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| format!("10.0.0.{i}:8080").into_bytes())
            .collect()
    }

    fn shares(table: &Maglev, hosts: usize) -> Vec<usize> {
        let mut counts = vec![0; hosts];
        for &h in table.table.iter() {
            counts[h as usize] += 1;
        }
        counts
    }

    #[test]
    fn empty_or_zero_weight_builds_nothing() {
        assert!(Maglev::build(&[], &[]).is_none());
        assert!(Maglev::build(&[b"a"], &[0]).is_none());
    }

    #[test]
    fn equal_weights_get_near_equal_shares() {
        let k = keys(5);
        let refs: Vec<&[u8]> = k.iter().map(Vec::as_slice).collect();
        let t = Maglev::build(&refs, &[1; 5]).unwrap();
        for share in shares(&t, 5) {
            let ideal = TABLE_SIZE / 5;
            assert!(share.abs_diff(ideal) <= 1, "{share} vs {ideal}");
        }
    }

    #[test]
    fn weights_scale_shares() {
        let k = keys(3);
        let refs: Vec<&[u8]> = k.iter().map(Vec::as_slice).collect();
        let t = Maglev::build(&refs, &[1, 2, 4]).unwrap();
        let s = shares(&t, 3);
        let ratio_1 = s[1] as f64 / s[0] as f64;
        let ratio_2 = s[2] as f64 / s[0] as f64;
        assert!((ratio_1 - 2.0).abs() < 0.05, "{s:?}");
        assert!((ratio_2 - 4.0).abs() < 0.05, "{s:?}");
    }

    #[test]
    fn removing_a_host_moves_little_else() {
        let k = keys(10);
        let all: Vec<&[u8]> = k.iter().map(Vec::as_slice).collect();
        let before = Maglev::build(&all, &[1; 10]).unwrap();
        // Remove host 3; indices above it shift down by one.
        let rest: Vec<&[u8]> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3)
            .map(|(_, k)| *k)
            .collect();
        let after = Maglev::build(&rest, &[1; 9]).unwrap();
        let mut moved_other = 0usize;
        for (b, a) in before.table.iter().zip(after.table.iter()) {
            let b = *b as usize;
            if b == 3 {
                continue;
            }
            let b_after = if b > 3 { b - 1 } else { b };
            if *a as usize != b_after {
                moved_other += 1;
            }
        }
        // The paper reports a few percent disruption; allow 10%.
        assert!(moved_other < TABLE_SIZE / 10, "{moved_other} slots moved");
    }

    #[test]
    fn lookup_is_deterministic_across_builds() {
        let k = keys(4);
        let refs: Vec<&[u8]> = k.iter().map(Vec::as_slice).collect();
        let a = Maglev::build(&refs, &[1; 4]).unwrap();
        let b = Maglev::build(&refs, &[1; 4]).unwrap();
        for key in ["alice", "bob", "carol"] {
            let h = hash_key(key.as_bytes());
            assert_eq!(a.lookup(h), b.lookup(h));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn small_tables_fill_completely(weights in prop::collection::vec(0u32..8, 1..6)) {
            let k = keys(weights.len());
            let refs: Vec<&[u8]> = k.iter().map(Vec::as_slice).collect();
            match Maglev::build_with_size(&refs, &weights, 251) {
                None => prop_assert!(weights.iter().all(|&w| w == 0)),
                Some(t) => {
                    for &h in t.table.iter() {
                        prop_assert!(weights[h as usize] > 0);
                    }
                }
            }
        }
    }
}
