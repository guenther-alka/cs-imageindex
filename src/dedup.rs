// Group photos into near-duplicate/burst-shot clusters using the
// perceptual hashes already computed by quality.rs. Simple O(n^2)
// union-find over Hamming distance -- fine for a few thousand photos per
// run; for very large single-folder batches, bucketing by hash prefix
// before comparing would be the first optimization if this becomes slow.

pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind { parent: (0..n).collect() }
    }
    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Returns a compact 1-based group id per input index -- two hashes within
/// `threshold` Hamming distance (transitively) share a group id. A photo
/// with no near-duplicates in the batch still gets its own (unique) id;
/// callers typically only show the id when the group has more than one
/// member.
pub fn group_by_hash(hashes: &[u64], threshold: u32) -> Vec<usize> {
    let n = hashes.len();
    let mut uf = UnionFind::new(n);
    for i in 0..n {
        for j in (i + 1)..n {
            if hamming_distance(hashes[i], hashes[j]) <= threshold {
                uf.union(i, j);
            }
        }
    }
    let mut id_map = std::collections::HashMap::new();
    let mut next_id = 1usize;
    let mut result = vec![0usize; n];
    for (i, r) in result.iter_mut().enumerate() {
        let root = uf.find(i);
        let id = *id_map.entry(root).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        });
        *r = id;
    }
    result
}

/// Group sizes indexed by group id (1-based; index 0 unused).
pub fn group_sizes(groups: &[usize]) -> Vec<usize> {
    let max_id = groups.iter().copied().max().unwrap_or(0);
    let mut sizes = vec![0usize; max_id + 1];
    for &g in groups {
        sizes[g] += 1;
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_distance_of_identical_is_zero() {
        assert_eq!(hamming_distance(0x1234, 0x1234), 0);
    }

    #[test]
    fn hamming_distance_counts_differing_bits() {
        assert_eq!(hamming_distance(0b1010, 0b0010), 1);
        assert_eq!(hamming_distance(0b1111, 0b0000), 4);
    }

    #[test]
    fn near_hashes_group_together_transitively() {
        // a-b differ by 1 bit, b-c differ by 1 bit, a-c differ by 2 bits --
        // all three should end up in the same group at threshold 1 thanks
        // to transitivity (a~b, b~c => a,b,c together) even though a-c
        // alone exceeds the threshold.
        let a = 0b0000u64;
        let b = 0b0001u64;
        let c = 0b0011u64;
        let far = 0xFFFF_FFFF_FFFF_FFFFu64;
        let groups = group_by_hash(&[a, b, c, far], 1);
        assert_eq!(groups[0], groups[1]);
        assert_eq!(groups[1], groups[2]);
        assert_ne!(groups[0], groups[3]);
    }

    #[test]
    fn group_sizes_counts_members() {
        let groups = vec![1, 1, 2, 1];
        let sizes = group_sizes(&groups);
        assert_eq!(sizes[1], 3);
        assert_eq!(sizes[2], 1);
    }
}
