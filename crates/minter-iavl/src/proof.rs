use crate::{Content, Hash, MAX_HEIGHT, Node, bytes, empty_hash, nonnegative, varint};
use anyhow::{Context, Result, bail, ensure};
use ics23::{ExistenceProof, HashOp, InnerOp, LeafOp, LengthOp};
use prost::Message;

// Two adjacent leaves can each be as large as Go's 64 MB snapshot item limit.
pub const MAX_PROOF_BYTES: usize = 128 * 1024 * 1024;
const HASH_LENGTH: u8 = 32;

impl Node<'_> {
    /// Encode a leaf and its path from the leaf to the root as Go ICS23.
    pub fn existence_proof(&self, path: Vec<InnerOp>) -> Result<ExistenceProof> {
        let Content::Leaf(value) = self.content else {
            bail!("an IAVL existence proof must start at a leaf");
        };
        let mut prefix = Vec::new();
        self.prefix(&mut prefix);
        Ok(ExistenceProof {
            key: self.key.to_vec(),
            value: value.to_vec(),
            leaf: Some(LeafOp {
                hash: HashOp::Sha256 as i32,
                prehash_key: HashOp::NoHash as i32,
                prehash_value: HashOp::Sha256 as i32,
                length: LengthOp::VarProto as i32,
                prefix,
            }),
            path,
        })
    }

    /// Encode the commitment step when descending into the chosen child.
    pub fn inner_op(&self, into_left: bool) -> Result<InnerOp> {
        let Content::Branch { left, right } = self.content else {
            bail!("an IAVL inner operation requires a branch");
        };
        let mut prefix = Vec::new();
        let mut suffix = Vec::new();
        self.prefix(&mut prefix);
        prefix.push(HASH_LENGTH);
        if into_left {
            suffix.push(HASH_LENGTH);
            suffix.extend(right);
        } else {
            prefix.extend(left);
            prefix.push(HASH_LENGTH);
        }
        Ok(InnerOp {
            hash: HashOp::Sha256 as i32,
            prefix,
            suffix,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Metadata {
    height: u8,
    size: u64,
    version: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Side {
    Left,
    Right,
}

#[derive(Eq, PartialEq)]
struct Step {
    meta: Metadata,
    side: Side,
    sibling: Hash,
    child: Hash,
}

struct Member<'a> {
    key: &'a [u8],
    value: &'a [u8],
    path: Vec<Step>,
}

/// Verify a Go Minter IAVL ICS23 proof against a caller-authenticated root.
/// A successful absence proof returns None. An empty committed value is Some.
/// The version is the IAVL version for this root, and can differ from block height.
pub fn verify_proof(
    root: Hash,
    version: u64,
    key: &[u8],
    mut encoded: &[u8],
) -> Result<Option<Vec<u8>>> {
    ensure!(
        version > 0 && version <= i64::MAX as u64,
        "invalid IAVL version"
    );
    ensure!(
        encoded.len() <= MAX_PROOF_BYTES,
        "IAVL proof exceeds the size limit"
    );
    let (tag, proof) = length_field(&mut encoded)?;
    ensure!(encoded.is_empty(), "expected one ICS23 commitment proof");
    match tag {
        1 => {
            let leaf = membership(root, version, proof)?;
            ensure!(leaf.key == key, "IAVL membership key differs");
            Ok(Some(leaf.value.to_vec()))
        }
        2 => {
            nonmembership(root, version, key, proof)?;
            Ok(None)
        }
        _ => bail!("unsupported ICS23 proof type"),
    }
}

fn membership(root: Hash, version: u64, mut input: &[u8]) -> Result<Member<'_>> {
    let (mut key, mut value, mut leaf) = (None, None, None);
    let mut inner = Vec::new();
    while !input.is_empty() {
        let (tag, data) = length_field(&mut input)?;
        match tag {
            1 => single(&mut key, data)?,
            2 => single(&mut value, data)?,
            3 => single(&mut leaf, data)?,
            4 => {
                ensure!(
                    inner.len() < usize::from(MAX_HEIGHT),
                    "IAVL proof path is too long"
                );
                inner.push(parse_inner(data, version)?);
            }
            _ => bail!("unknown ICS23 existence field"),
        }
    }
    let key = key.unwrap_or_default();
    let value = value.unwrap_or_default();
    let mut meta = parse_leaf(leaf.context("missing ICS23 leaf operation")?, version)?;
    let mut scratch = Vec::new();
    let mut hash = meta.node(key, Content::Leaf(value)).hash(&mut scratch);
    let mut path = Vec::with_capacity(inner.len());
    for (parent, side, sibling) in inner {
        ensure!(
            parent.height > meta.height
                && parent.height - meta.height <= 2
                && parent.size > meta.size
                && parent.version >= meta.version,
            "invalid IAVL proof parent metadata"
        );
        let (left, right) = match side {
            Side::Left => (hash, sibling),
            Side::Right => (sibling, hash),
        };
        path.push(Step {
            meta: parent,
            side,
            sibling,
            child: hash,
        });
        hash = parent
            .node(&[], Content::Branch { left, right })
            .hash(&mut scratch);
        meta = parent;
    }
    ensure!(
        hash == root,
        "IAVL proof root differs from the trusted root"
    );
    Ok(Member { key, value, path })
}

fn nonmembership(root: Hash, version: u64, key: &[u8], mut input: &[u8]) -> Result<()> {
    let (mut proof_key, mut left, mut right) = (None, None, None);
    while !input.is_empty() {
        let (tag, data) = length_field(&mut input)?;
        match tag {
            1 => single(&mut proof_key, data)?,
            2 => single(&mut left, data)?,
            3 => single(&mut right, data)?,
            _ => bail!("unknown ICS23 nonexistence field"),
        }
    }
    ensure!(
        proof_key.unwrap_or_default() == key,
        "IAVL absence key differs"
    );
    let left = left
        .map(|proof| membership(root, version, proof))
        .transpose()?;
    let right = right
        .map(|proof| membership(root, version, proof))
        .transpose()?;
    ensure!(
        left.as_ref().is_none_or(|proof| proof.key < key)
            && right.as_ref().is_none_or(|proof| key < proof.key),
        "IAVL absence neighbors do not bound the key"
    );
    let adjacent = match (left, right) {
        (Some(left), Some(right)) => neighboring_paths(&left.path, &right.path),
        (Some(left), None) => left.path.iter().all(|step| step.side == Side::Right),
        (None, Some(right)) => right.path.iter().all(|step| step.side == Side::Left),
        (None, None) => root == empty_hash(),
    };
    ensure!(adjacent, "IAVL absence proof lacks adjacent neighbors");
    Ok(())
}

fn neighboring_paths(left: &[Step], right: &[Step]) -> bool {
    let mut left = left.iter().rev();
    let mut right = right.iter().rev();
    // Walk down the common path to the branch containing both neighbors.
    while let (Some(a), Some(b)) = (left.next(), right.next()) {
        if a == b {
            continue;
        }
        return a.meta == b.meta
            && a.side == Side::Left
            && b.side == Side::Right
            && a.sibling == b.child
            && b.sibling == a.child
            && left.all(|step| step.side == Side::Right)
            && right.all(|step| step.side == Side::Left);
    }
    false
}

impl Metadata {
    fn node<'a>(self, key: &'a [u8], content: Content<'a>) -> Node<'a> {
        Node {
            height: self.height,
            size: self.size,
            version: self.version,
            key,
            content,
        }
    }
}

fn read_metadata(input: &mut &[u8], version: u64) -> Result<Metadata> {
    let height = u8::try_from(nonnegative(input)?)?;
    let size = nonnegative(input)?;
    let node_version = nonnegative(input)?;
    ensure!(
        height <= MAX_HEIGHT && (1..=version).contains(&node_version),
        "invalid IAVL proof node height or version"
    );
    Ok(Metadata {
        height,
        size,
        version: node_version,
    })
}

fn parse_leaf(raw: &[u8], version: u64) -> Result<Metadata> {
    let op: LeafOp = canonical_op(raw)?;
    ensure!(
        op.hash == HashOp::Sha256 as i32
            && op.prehash_key == HashOp::NoHash as i32
            && op.prehash_value == HashOp::Sha256 as i32
            && op.length == LengthOp::VarProto as i32,
        "invalid IAVL leaf operation"
    );
    let mut prefix = op.prefix.as_slice();
    let meta = read_metadata(&mut prefix, version)?;
    ensure!(
        meta.height == 0 && meta.size == 1 && prefix.is_empty(),
        "invalid IAVL leaf prefix"
    );
    Ok(meta)
}

fn parse_inner(raw: &[u8], version: u64) -> Result<(Metadata, Side, Hash)> {
    let op: InnerOp = canonical_op(raw)?;
    ensure!(
        op.hash == HashOp::Sha256 as i32,
        "invalid IAVL inner operation"
    );
    let mut prefix = op.prefix.as_slice();
    let meta = read_metadata(&mut prefix, version)?;
    ensure!(
        meta.height > 0 && meta.size > 1,
        "invalid IAVL inner metadata"
    );
    let (side, sibling) = match (prefix, op.suffix.as_slice()) {
        ([HASH_LENGTH], [HASH_LENGTH, sibling @ ..]) => (Side::Left, sibling),
        ([HASH_LENGTH, sibling @ .., HASH_LENGTH], []) => (Side::Right, sibling),
        _ => bail!("invalid IAVL inner prefix or suffix"),
    };
    Ok((
        meta,
        side,
        sibling.try_into().context("invalid IAVL sibling hash")?,
    ))
}

fn canonical_op<T: Message + Default>(raw: &[u8]) -> Result<T> {
    ensure!(raw.len() <= 128, "ICS23 operation exceeds the size limit");
    let op = T::decode(raw)?;
    ensure!(op.encode_to_vec() == raw, "noncanonical ICS23 operation");
    Ok(op)
}

fn single<'a>(field: &mut Option<&'a [u8]>, data: &'a [u8]) -> Result<()> {
    ensure!(field.replace(data).is_none(), "duplicate ICS23 field");
    Ok(())
}

// Stream the length-delimited messages before allocation. A malicious proof
// cannot make prost allocate an unbounded repeated list of InnerOp messages.
fn length_field<'a>(input: &mut &'a [u8]) -> Result<(u64, &'a [u8])> {
    let tag = varint(input)?;
    ensure!(tag & 7 == 2, "invalid ICS23 proof wire type");
    Ok((tag >> 3, bytes(input)?))
}
