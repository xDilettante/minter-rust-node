use crate::{KeyRead, load_node, read_first_key, read_last_key, read_verified_node};
use anyhow::{Context, Result};
use minter_iavl::{
    Content, Hash, MAX_HEIGHT,
    ics23::{CommitmentProof, ExistenceProof, NonExistenceProof, commitment_proof::Proof},
};
use prost::Message;

pub struct StateProof {
    pub value: Option<Vec<u8>>,
    pub proof: Vec<u8>,
}

pub(crate) fn prove(
    db: &impl KeyRead,
    root: Option<Hash>,
    version: u64,
    key: &[u8],
) -> Result<StateProof> {
    let mut value = None;
    let mut absent = NonExistenceProof {
        key: key.to_vec(),
        left: None,
        right: None,
    };
    let proof = if let Some(root) = root {
        if let Some(member) = existence(db, root, version, key)? {
            value = Some(member.value.clone());
            Proof::Exist(member)
        } else {
            let mut scratch = Vec::new();
            let left = read_last_key(db, root, version, MAX_HEIGHT + 1, Some(key), &mut scratch)?;
            let right = read_first_key(db, root, version, MAX_HEIGHT + 1, Some(key), &mut scratch)?;
            let witness = |key: Vec<u8>| {
                existence(db, root, version, &key)?.context("missing IAVL proof neighbor")
            };
            absent.left = left.map(witness).transpose()?;
            absent.right = right.map(witness).transpose()?;
            Proof::Nonexist(absent)
        }
    } else {
        Proof::Nonexist(absent)
    };
    Ok(StateProof {
        value,
        proof: CommitmentProof { proof: Some(proof) }.encode_to_vec(),
    })
}

fn existence(
    db: &impl KeyRead,
    mut hash: Hash,
    version: u64,
    key: &[u8],
) -> Result<Option<ExistenceProof>> {
    let mut max_height = MAX_HEIGHT + 1;
    let mut path = Vec::new();
    let mut scratch = Vec::new();
    loop {
        let raw = load_node(db, hash)?;
        let node = read_verified_node(&raw, hash, version, max_height, &mut scratch)?;
        match node.content {
            Content::Leaf(_) => {
                if key != node.key {
                    return Ok(None);
                }
                path.reverse();
                return Ok(Some(node.existence_proof(path)?));
            }
            Content::Branch { left, right } => {
                let into_left = key < node.key;
                path.push(node.inner_op(into_left)?);
                hash = if into_left { left } else { right };
                max_height = node.height;
            }
        }
    }
}
