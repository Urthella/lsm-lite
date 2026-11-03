//! Bloom filters, so a lookup can skip a table that can't hold the key.

/// 10 bits a key with 7 probes gives about a 1% false positive rate.
const BITS_PER_KEY: usize = 10;
const PROBES: u64 = 7;

/// Builds a filter from the keys' hashes.
pub fn build(hashes: &[u64]) -> Vec<u8> {
    let mut filter = vec![0u8; (hashes.len() * BITS_PER_KEY).max(64).div_ceil(8)];
    let bits = filter.len() * 8;
    for &hash in hashes {
        for bit in probes(hash, bits) {
            filter[bit / 8] |= 1 << (bit % 8);
        }
    }
    filter
}

/// False means the key is certainly absent. True means it may be there.
pub fn may_contain(filter: &[u8], key: &[u8]) -> bool {
    probes(hash(key), filter.len() * 8).all(|bit| filter[bit / 8] & (1 << (bit % 8)) != 0)
}

/// Double hashing: probe `i` is `h1 + i * h2`.
fn probes(hash: u64, bits: usize) -> impl Iterator<Item = usize> {
    let (h1, h2) = (hash & 0xffff_ffff, hash >> 32);
    (0..PROBES).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % bits as u64) as usize)
}

/// FNV-1a with a final mix, since filters are stored on disk and std's hasher can change between
/// Rust versions.
pub fn hash(key: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &byte in key {
        h = (h ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h = (h ^ (h >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
    h = (h ^ (h >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_about_one_percent_false_positives() {
        let keys: Vec<Vec<u8>> = (0..10_000)
            .map(|i| format!("key{i}").into_bytes())
            .collect();
        let filter = build(&keys.iter().map(|k| hash(k)).collect::<Vec<_>>());
        assert!(keys.iter().all(|k| may_contain(&filter, k)));
        let false_positives = (0..10_000)
            .filter(|i| may_contain(&filter, format!("other{i}").as_bytes()))
            .count();
        assert!(false_positives < 200, "{false_positives} of 10000");
    }

    #[test]
    fn an_empty_table_still_gets_a_working_filter() {
        let filter = build(&[]);
        assert!(!may_contain(&filter, b"anything"));
    }
}
