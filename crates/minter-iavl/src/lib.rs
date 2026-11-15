use anyhow::{Result, bail, ensure};
pub use ics23;
use sha2::{Digest, Sha256};

mod proof;
pub use proof::verify_proof;

pub type Hash = [u8; 32];
pub const MAX_HEIGHT: u8 = 127;
pub const QUERY_PATH: &str = "/iavl/key";
pub const PROOF_OP_TYPE: &str = "ics23:iavl";

#[derive(Clone, Copy)]
pub enum Content<'a> {
    Leaf(&'a [u8]),
    Branch { left: Hash, right: Hash },
}

pub struct Node<'a> {
    pub height: u8,
    pub size: u64,
    pub version: u64,
    pub key: &'a [u8],
    pub content: Content<'a>,
}

impl<'a> Node<'a> {
    pub fn prefix(&self, out: &mut Vec<u8>) {
        out.clear();
        // Go encodes the signed, non-negative IAVL fields with binary.PutVarint.
        put_varint(u64::from(self.height) << 1, out);
        put_varint(self.size << 1, out);
        put_varint(self.version << 1, out);
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        self.prefix(out);
        put_bytes(self.key, out);
        match self.content {
            Content::Leaf(value) => put_bytes(value, out),
            Content::Branch { left, right } => {
                put_bytes(&left, out);
                put_bytes(&right, out);
            }
        }
    }

    pub fn hash(&self, scratch: &mut Vec<u8>) -> Hash {
        self.prefix(scratch);
        match self.content {
            Content::Leaf(value) => {
                put_bytes(self.key, scratch);
                put_bytes(&Sha256::digest(value), scratch);
            }
            Content::Branch { left, right } => {
                // Inner separator keys are stored but excluded from the IAVL hash.
                put_bytes(&left, scratch);
                put_bytes(&right, scratch);
            }
        }
        Sha256::digest(scratch).into()
    }

    pub fn decode(mut input: &'a [u8]) -> Result<Self> {
        let height = u8::try_from(nonnegative(&mut input)?)?;
        let size = nonnegative(&mut input)?;
        let version = nonnegative(&mut input)?;
        ensure!(
            height <= MAX_HEIGHT && version > 0,
            "invalid IAVL node metadata"
        );
        let key = bytes(&mut input)?;
        let content = if height == 0 {
            ensure!(size == 1, "invalid IAVL leaf size");
            Content::Leaf(bytes(&mut input)?)
        } else {
            ensure!(size > 1, "invalid IAVL branch size");
            Content::Branch {
                left: bytes(&mut input)?.try_into()?,
                right: bytes(&mut input)?.try_into()?,
            }
        };
        ensure!(input.is_empty(), "trailing bytes in IAVL node");
        Ok(Self {
            height,
            size,
            version,
            key,
            content,
        })
    }
}

pub fn empty_hash() -> Hash {
    Sha256::digest([]).into()
}

pub fn node_key(hash: Hash) -> [u8; 33] {
    let mut key = [b'n'; 33];
    key[1..].copy_from_slice(&hash);
    key
}

pub fn root_key(height: u64) -> [u8; 9] {
    let mut key = [b'r'; 9];
    key[1..].copy_from_slice(&height.to_be_bytes());
    key
}

fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    put_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0;
    for index in 0..10 {
        let Some((&byte, rest)) = input.split_first() else {
            bail!("truncated IAVL varint");
        };
        *input = rest;
        ensure!(index != 9 || byte <= 1, "IAVL varint overflow");
        value |= u64::from(byte & 127) << (index * 7);
        if byte < 128 {
            ensure!(index == 0 || byte != 0, "non-canonical IAVL varint");
            return Ok(value);
        }
    }
    bail!("IAVL varint overflow")
}

fn nonnegative(input: &mut &[u8]) -> Result<u64> {
    let value = varint(input)?;
    ensure!(value & 1 == 0, "negative IAVL node metadata");
    Ok(value >> 1)
}

fn bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = usize::try_from(varint(input)?)?;
    let Some((bytes, rest)) = input.split_at_checked(len) else {
        bail!("truncated IAVL bytes");
    };
    *input = rest;
    Ok(bytes)
}
