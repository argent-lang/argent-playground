//! A compact sparse Merkle set using iterative traversal.
//!
//! The tree has a fixed logical depth of 256. `Shortcut` nodes compress paths
//! structurally and do not add hashes to the commitment.

pub type Key = [u8; 32];
pub type Hash = [u8; 32];

const DEPTH: usize = 256;
// pub const FAST_INSERT_HASH_BUDGET: usize = 32;
// pub const SLOW_INSERT_HASH_BUDGET: usize = 512;
const SMT_NODE_DOMAIN: [u8; 32] =
    [0x53, 0x6d, 0x74, 0x4e, 0x6f, 0x64, 0x65, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// A compact sparse Merkle set.
///
/// Nodes refer to each other by index. This keeps insert, lookup, hashing, and
/// proof construction as ordinary loops rather than recursive calls.
#[derive(Debug, Default)]
pub struct CompactSparseMerkleSet {
    nodes: Vec<Node>,
    root: Option<usize>,
}

impl CompactSparseMerkleSet {
    /// Creates an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether `key` is present.
    pub fn contains(&self, key: &Key) -> bool {
        let Some(mut node_index) = self.root else {
            return false;
        };
        let mut depth = 0;

        // A path consumes at least one logical level per branch or shortcut,
        // then ends at a leaf.
        for _ in 0..=DEPTH {
            match &self.nodes[node_index] {
                Node::Leaf { key: existing, .. } => return existing == key,
                Node::Branch { left, right } => {
                    node_index = if Self::bit(key, depth) == 0 { *left } else { *right };
                    depth += 1;
                }
                Node::Shortcut { skip, child } => {
                    let skip = usize::from(*skip);
                    let representative = self.representative_key(*child);
                    if Self::common_bits(key, representative, depth, skip) != skip {
                        return false;
                    }

                    node_index = *child;
                    depth += skip;
                }
            }
        }

        false
    }

    /// Inserts a new key and an opaque payload returned with proofs that select
    /// this leaf as their witness.
    ///
    /// Existing keys are not inserted again. In that case this returns `false`.
    ///
    /// Example, insert `01011100` with this initial structure:
    /// ```
    /// Shortcut(skip=8, path=01010110)
    /// └── Leaf(01010110)
    /// ```
    ///
    /// Result:
    /// ```
    /// Shortcut(skip=4, path=0101)
    /// └── Branch(depth=4)
    ///     ├── 0: Shortcut(skip=3, path=110)
    ///     │      └── Leaf(01010110)
    ///     │
    ///     └── 1: Shortcut(skip=3, path=100)
    ///            └── Leaf(01011100)
    /// ```
    pub fn insert(&mut self, key: Key, witness: Vec<u8>) -> bool {
        // root still is empty
        let Some(mut node_index) = self.root else {
            debug_assert!(self.nodes.is_empty());
            self.nodes.push(Node::Leaf { key, witness });
            self.nodes.push(Node::Shortcut { skip: DEPTH as u16, child: 0 });
            self.root = Some(1);
            return true;
        };

        let mut depth = 0;

        // at most 257 iterations (256 branches/shortcuts, and one final leaf)
        for _ in 0..=DEPTH {
            match &self.nodes[node_index] {
                Node::Leaf { key: existing, .. } => {
                    assert_eq!(depth, DEPTH);
                    assert_eq!(*existing, key);
                    return false;
                }
                Node::Branch { left, right } => {
                    node_index = if Self::bit(&key, depth) == 0 { *left } else { *right };
                    depth += 1;
                }
                Node::Shortcut { skip, child } => {
                    let skip = usize::from(*skip);
                    let child = *child;
                    let representative = *self.representative_key(child);
                    let shared = Self::common_bits(&key, &representative, depth, skip);

                    // continue through the shortcut
                    if shared == skip {
                        node_index = child;
                        depth += skip;
                        continue;
                    }

                    // depth where to create a new branch
                    let branch_depth = depth + shared;
                    let existing_side = Self::bit(&representative, branch_depth);

                    let existing_suffix = skip - shared - 1;
                    // divergence occurs at the final shortcut bit
                    let existing = if existing_suffix == 0 {
                        child
                    // divergence occurs before final, take the suffix and create a new shortcut
                    // e.g.: [matching][branch_bit][remaining_suffix]
                    } else {
                        let index = self.nodes.len();
                        self.nodes.push(Node::Shortcut { skip: existing_suffix as u16, child });
                        index
                    };

                    // create the leaf for the newly inserted key
                    let new_leaf = self.nodes.len();
                    self.nodes.push(Node::Leaf { key, witness });

                    // the branch consumes the divergence bit
                    // every bit after it belongs to the new key's path down to depth 256
                    let new_suffix = DEPTH - branch_depth - 1;
                    // divergence occurs at the final tree bit
                    let new = if new_suffix == 0 {
                        new_leaf
                    // otherwise compress the remaining path to the new leaf
                    // e.g.: [branch_bit][remaining_new_key_suffix]
                    } else {
                        let index = self.nodes.len();
                        self.nodes.push(Node::Shortcut { skip: new_suffix as u16, child: new_leaf });
                        index
                    };

                    let (left, right) = if existing_side == 0 { (existing, new) } else { (new, existing) };

                    if shared == 0 {
                        // Reuse the old shortcut slot for the new branch.
                        self.nodes[node_index] = Node::Branch { left, right };
                    } else {
                        let branch = self.nodes.len();
                        self.nodes.push(Node::Branch { left, right });
                        self.nodes[node_index] = Node::Shortcut { skip: shared as u16, child: branch };
                    }

                    return true;
                }
            }
        }

        false
    }

    pub fn root_hash(&self) -> Hash {
        self.root.map_or_else(Self::empty_hash, |root| self.node_hash(root, 0))
    }

    /// Creates a compact membership or non-membership proof for `key`.
    pub fn prove(&self, key: &Key) -> CompactProof {
        let mut proof = CompactProof { key: *key, witness: None, branches: Vec::new() };
        let Some(mut node_index) = self.root else {
            return proof;
        };
        let mut depth = 0;
        let mut path = *key;

        for _ in 0..=DEPTH {
            match &self.nodes[node_index] {
                Node::Leaf { key, witness } => {
                    proof.witness = Some((*key, witness.clone()));
                    return proof;
                }
                Node::Branch { left, right } => {
                    if Self::bit(&path, depth) == 0 {
                        proof.branches.push(ProofBranch { depth: depth as u16, sibling: self.node_hash(*right, depth + 1) });
                        node_index = *left;
                    } else {
                        proof.branches.push(ProofBranch { depth: depth as u16, sibling: self.node_hash(*left, depth + 1) });
                        node_index = *right;
                    }
                    depth += 1;
                }
                Node::Shortcut { skip, child } => {
                    let skip = usize::from(*skip);
                    let representative = *self.representative_key(*child);
                    let shared = Self::common_bits(key, &representative, depth, skip);

                    if shared == skip {
                        node_index = *child;
                        depth += skip;
                        continue;
                    }

                    // continue to real leaf
                    path = representative;
                    node_index = *child;
                    depth += skip;
                }
            }
        }

        proof
    }

    /// Follows left/shortcut links until it reaches a descendant leaf.
    ///
    /// Every key below a shortcut shares its compressed path bits, so any leaf
    /// below it can represent that path.
    fn representative_key(&self, mut node_index: usize) -> &Key {
        for _ in 0..=DEPTH {
            match &self.nodes[node_index] {
                Node::Leaf { key, .. } => return key,
                Node::Branch { left, .. } => node_index = *left,
                Node::Shortcut { child, .. } => node_index = *child,
            }
        }

        unreachable!("a valid path reaches a leaf within the fixed depth")
    }

    /// Hashes a subtree iteratively.
    ///
    /// Each stack entry contains a node, its logical depth, and whether its
    /// children have already been visited. Hashes produced by completed
    /// children are kept on a second stack.
    fn node_hash(&self, node_index: usize, depth: usize) -> Hash {
        let mut nodes = vec![(node_index, depth, false)];
        let mut hashes = Vec::new();

        // Each stored node is scheduled at most twice: once before its
        // children and once after them.
        for _ in 0..self.nodes.len().saturating_mul(2) {
            let Some((node_index, depth, visited)) = nodes.pop() else {
                break;
            };

            match (&self.nodes[node_index], visited) {
                (Node::Leaf { key, .. }, _) => hashes.push(*key),
                (Node::Branch { left, right }, false) => {
                    nodes.push((node_index, depth, true));
                    nodes.push((*right, depth + 1, false));
                    nodes.push((*left, depth + 1, false));
                }
                (Node::Branch { .. }, true) => {
                    // Post-order traversal guarantees these two hashes exist.
                    let right = hashes.pop().expect("right child was visited");
                    let left = hashes.pop().expect("left child was visited");
                    hashes.push(Self::branch_hash(depth, &left, &right));
                }
                (Node::Shortcut { skip, child }, false) => {
                    nodes.push((node_index, depth, true));
                    nodes.push((*child, depth + usize::from(*skip), false));
                }
                (Node::Shortcut { .. }, true) => {}
            }
        }

        assert!(nodes.is_empty());
        assert!(hashes.len() == 1);
        hashes.pop().expect("the subtree produced one hash")
    }

    /// Maps a root-to-leaf depth to its byte and mask.
    ///
    /// Paths are encoded most-significant bit first within each byte: depth 0
    /// is `0x80`, depth 7 is `0x01`, and depth 8 starts the next byte at
    /// `0x80`. Keys and proof bitmaps must use the same ordering.
    fn bit_location(depth: usize) -> (usize, u8) {
        (depth / 8, 1 << (7 - depth % 8))
    }

    /// Returns a key bit in root-to-leaf order.
    fn bit(key: &Key, depth: usize) -> u8 {
        let (byte_index, bit_mask) = Self::bit_location(depth);
        u8::from(key[byte_index] & bit_mask != 0)
    }

    /// Counts equal bits inside one compressed path.
    fn common_bits(left: &Key, right: &Key, depth: usize, len: usize) -> usize {
        (0..len).take_while(|offset| Self::bit(left, depth + offset) == Self::bit(right, depth + offset)).count()
    }

    /// Hashes a compact sparse Merkle branch with `SMT_NODE_DOMAIN` over
    /// `depth_le_u16 || left || right`.
    fn branch_hash(depth: usize, left: &Hash, right: &Hash) -> Hash {
        assert!(depth < DEPTH);
        let mut bytes = [0; 66];
        bytes[..2].copy_from_slice(&(depth as u16).to_le_bytes());
        bytes[2..34].copy_from_slice(left);
        bytes[34..].copy_from_slice(right);
        *blake3::keyed_hash(&SMT_NODE_DOMAIN, &bytes).as_bytes()
    }

    /// Returns the commitment of an empty compact sparse Merkle set.
    fn empty_hash() -> Hash {
        *blake3::hash(&[]).as_bytes()
    }
}

/// a populated node in the tree `Vec<Node>`
#[derive(Debug)]
enum Node {
    Leaf { key: Key, witness: Vec<u8> },
    Branch { left: usize, right: usize },
    Shortcut { skip: u16, child: usize },
}

/// one real compact sparse Merkle branch on a leaf's membership path
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProofBranch {
    depth: u16,
    sibling: Hash,
}

/// A compact proof anchored by one leaf
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactProof {
    key: Key,
    witness: Option<(Key, Vec<u8>)>,
    branches: Vec<ProofBranch>,
}

impl CompactProof {
    /// Returns the existing leaf key and its opaque witness payload, or `None`
    /// for an empty-tree proof.
    pub fn witness(&self) -> Option<(Key, &[u8])> {
        self.witness.as_ref().map(|(key, witness)| (*key, witness.as_slice()))
    }

    /// Encodes a proof in leaf-to-root order.
    ///
    /// Layout:
    /// `leaf_key (32) || divergence_depth_u8 (1) || lower_count_u8 (1)`
    /// `|| (branch_depth_u8 (1) || side_offset_u8 (1) || sibling (32))*`
    ///
    /// An empty-tree proof is empty. Non-empty proofs contain a 34-byte header
    /// followed by zero or more 34-byte branch records.
    /// The witness payload is returned by [`Self::witness`] and is not included
    /// in this fixed-width encoding.
    pub fn encode(&self) -> Vec<u8> {
        let Some((leaf_key, _)) = &self.witness else {
            return Vec::new();
        };

        let divergence = CompactSparseMerkleSet::common_bits(&self.key, leaf_key, 0, DEPTH);
        let lower_count = self.branches.iter().filter(|branch| usize::from(branch.depth) > divergence).count();
        let mut bytes = Vec::with_capacity(34 + self.branches.len() * 34);
        bytes.extend_from_slice(leaf_key); // 32
        // Valid insertion proofs have a divergence below DEPTH. A membership
        // proof's DEPTH sentinel truncates here and fails the on-chain
        // opposite-bit check.
        bytes.push(divergence as u8); // 1
        bytes.push(lower_count as u8); // 1
        for branch in self.branches.iter().rev() {
            bytes.push(branch.depth as u8); // 1
            bytes.push(CompactSparseMerkleSet::bit(leaf_key, usize::from(branch.depth)) * 32); // 1
            bytes.extend_from_slice(&branch.sibling); // 32
        }
        bytes
    }

    /// Verifies membership when `present` is true or non-membership otherwise.
    #[allow(dead_code)]
    pub fn verify(&self, root: &Hash, key: &Key, present: bool) -> bool {
        if self.key != *key || !self.has_valid_branch_order() {
            return false;
        }

        let Some((leaf_key, _)) = &self.witness else {
            return !present && self.branches.is_empty() && *root == CompactSparseMerkleSet::empty_hash();
        };
        if (*leaf_key == *key) != present {
            return false;
        }

        let mut current = *leaf_key;
        for branch in self.branches.iter().rev() {
            let depth = usize::from(branch.depth);
            current = if CompactSparseMerkleSet::bit(leaf_key, depth) == 0 {
                CompactSparseMerkleSet::branch_hash(depth, &current, &branch.sibling)
            } else {
                CompactSparseMerkleSet::branch_hash(depth, &branch.sibling, &current)
            };
        }

        current == *root
    }

    /// Checks that branch depths are canonical root-to-leaf positions.
    fn has_valid_branch_order(&self) -> bool {
        self.branches.iter().all(|branch| usize::from(branch.depth) < DEPTH)
            && self.branches.windows(2).all(|pair| pair[0].depth < pair[1].depth)
    }
}
