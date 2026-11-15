//! Verification of Tendermint 0.34 blocks and their successor commits.

use crate::ReceivedBlock;
use anyhow::{Context, Result, bail, ensure};
use curve25519_dalek_ng::{
    edwards::{CompressedEdwardsY, EdwardsPoint},
    scalar::Scalar,
};
use prost::Message;
use sha2::{Digest, Sha512};
use tendermint::{
    Hash, PublicKey, Signature, Time,
    block::{self, Commit, CommitSig, Height},
    chain,
    crypto::{
        default::Sha256,
        signature::{self, Error},
    },
    merkle,
    validator::Set,
    vote::{SignedVote, Type, Vote},
};
use tendermint_proto::v0_34::types;

/// Ed25519 verification used by Go Tendermint 0.34. Go compares the exact
/// encoded R of the uncofactored equation, unlike ZIP215.
/// https://github.com/golang/go/blob/go1.24.0/src/crypto/internal/fips140/ed25519/ed25519.go#L281-L316
pub struct GoEd25519;

impl signature::Verifier for GoEd25519 {
    fn verify(pubkey: PublicKey, msg: &[u8], signature: &Signature) -> Result<(), Error> {
        let key = pubkey.ed25519().ok_or(Error::UnsupportedKeyType)?;
        let key_bytes = key
            .as_bytes()
            .try_into()
            .map_err(|_| Error::MalformedPublicKey)?;
        let point = CompressedEdwardsY(key_bytes)
            .decompress()
            .ok_or(Error::MalformedPublicKey)?;
        let (r, s) = signature
            .as_bytes()
            .split_at_checked(32)
            .ok_or(Error::MalformedSignature)?;
        let scalar =
            Scalar::from_canonical_bytes(s.try_into().map_err(|_| Error::MalformedSignature)?)
                .ok_or(Error::VerificationFailed)?;
        let mut hash = Sha512::new();
        hash.update(r);
        hash.update(key.as_bytes());
        hash.update(msg);
        let challenge = Scalar::from_bytes_mod_order_wide(&hash.finalize().into());
        let encoded =
            EdwardsPoint::vartime_double_scalar_mul_basepoint(&challenge, &(-point), &scalar)
                .compress();
        if encoded.as_bytes() == r {
            Ok(())
        } else {
            Err(Error::VerificationFailed)
        }
    }
}

pub(crate) fn block_id(header: &block::Header, encoded: &[u8]) -> Result<block::Id> {
    // Tendermint 0.34 MakePartSet hashes the protobuf block in 64 KiB chunks.
    let parts: Vec<_> = encoded.chunks(crate::consensus::PART_SIZE).collect();
    let hash = Hash::Sha256(merkle::simple_hash_from_byte_vectors::<Sha256>(&parts));
    Ok(block::Id {
        hash: header.hash(),
        part_set_header: block::parts::Header::new(parts.len().try_into()?, hash)?,
    })
}

pub fn commit_hash(commit: Option<&Commit>) -> Hash {
    let signatures = commit
        .into_iter()
        .flat_map(|commit| &commit.signatures)
        .map(|signature| types::CommitSig::from(signature.clone()).encode_to_vec())
        .collect::<Vec<_>>();
    Hash::Sha256(merkle::simple_hash_from_byte_vectors::<Sha256>(&signatures))
}

/// Verify the successor's LastCommit with the validators of the block it signs.
pub fn verify_link<'a>(
    block: &ReceivedBlock,
    successor: &'a ReceivedBlock,
    validators: &Set,
) -> Result<&'a Commit> {
    let header = &block.block().header;
    let next = &successor.block().header;
    ensure!(
        next.chain_id == header.chain_id
            && header.height.value().checked_add(1) == Some(next.height.value()),
        "nonadjacent block or unexpected chain"
    );
    ensure!(
        next.last_block_id == Some(block.id()),
        "last BlockID differs at height {}",
        next.height
    );
    let commit = successor
        .block()
        .last_commit
        .as_ref()
        .context("missing LastCommit")?;
    ensure!(
        next.last_commit_hash == Some(commit_hash(Some(commit))),
        "LastCommitHash differs at height {}",
        next.height
    );
    verify_commit(
        &header.chain_id,
        header.height,
        block.id(),
        validators,
        commit,
    )?;
    Ok(commit)
}

/// Verify every non-absent signature and require strictly more than 2/3 voting power.
/// Nil precommits are checked but contribute no power to the committed block.
pub fn verify_commit(
    chain: &chain::Id,
    height: Height,
    block: block::Id,
    validators: &Set,
    commit: &Commit,
) -> Result<()> {
    ensure!(
        commit.height == height && commit.block_id == block,
        "commit names a different block"
    );
    ensure!(
        commit.signatures.len() == validators.validators().len(),
        "wrong number of commit signatures"
    );
    let mut signed_power = 0;
    for (index, (signature, validator)) in commit
        .signatures
        .iter()
        .zip(validators.validators())
        .enumerate()
    {
        let (block_id, address, timestamp, signature) = match signature {
            CommitSig::BlockIdFlagAbsent => continue,
            CommitSig::BlockIdFlagCommit {
                validator_address,
                timestamp,
                signature,
            } => {
                signed_power += validator.power();
                (Some(block), validator_address, timestamp, signature)
            }
            CommitSig::BlockIdFlagNil {
                validator_address,
                timestamp,
                signature,
            } => (None, validator_address, timestamp, signature),
        };
        ensure!(
            *address == validator.address,
            "wrong validator address at commit index {index}"
        );
        let vote = Vote {
            vote_type: Type::Precommit,
            height,
            round: commit.round,
            block_id,
            timestamp: Some(*timestamp),
            validator_address: *address,
            validator_index: index.try_into()?,
            signature: signature.clone(),
            extension: Vec::new(),
            extension_signature: None,
        };
        let signed =
            SignedVote::from_vote(vote, chain.clone()).context("missing precommit signature")?;
        validator
            .verify_signature::<GoEd25519>(&signed.sign_bytes(), signed.signature())
            .with_context(|| {
                format!("invalid precommit at height {height}, validator {address}")
            })?;
    }
    ensure!(
        signed_power * 3 > validators.total_voting_power().value() * 2,
        "insufficient voting power at height {height}"
    );
    Ok(())
}

/// Timestamp of the next block, derived from a verified commit.
pub fn median_time(commit: &Commit, validators: &Set) -> Result<Time> {
    let mut votes = commit
        .signatures
        .iter()
        .zip(validators.validators())
        .filter_map(|(signature, validator)| match signature {
            CommitSig::BlockIdFlagAbsent => None,
            CommitSig::BlockIdFlagCommit { timestamp, .. }
            | CommitSig::BlockIdFlagNil { timestamp, .. } => Some((*timestamp, validator.power())),
        })
        .collect::<Vec<_>>();
    let mut median = votes.iter().map(|(_, power)| power).sum::<u64>() / 2;
    votes.sort_by_key(|(time, _)| *time);
    for (timestamp, power) in votes {
        if median <= power {
            return Ok(timestamp);
        }
        median -= power;
    }
    bail!("commit has no vote timestamps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_consensus::SigningKey;
    use tendermint::{PublicKey, Signature, validator::Info};

    #[test]
    fn verifies_the_uncofactored_go_signature_equation() {
        use curve25519_dalek_ng::constants::{ED25519_BASEPOINT_POINT, EIGHT_TORSION};
        use tendermint::crypto::signature::Verifier;

        let seed = [7; 32];
        let key = SigningKey::from(seed);
        let public: PublicKey = key.verification_key().into();
        let message = b"Go Tendermint signature test";
        let valid = Signature::new(key.sign(message).to_bytes())
            .unwrap()
            .unwrap();
        GoEd25519::verify(public, message, &valid).unwrap();

        let expanded = Sha512::digest(seed);
        let mut secret: [u8; 32] = expanded[..32].try_into().unwrap();
        secret[0] &= 248;
        secret[31] = (secret[31] & 63) | 64;
        let secret = Scalar::from_bytes_mod_order(secret);
        let nonce = Scalar::from(91u64);
        let r = (ED25519_BASEPOINT_POINT * nonce + EIGHT_TORSION[1]).compress();
        let mut hash = Sha512::new();
        hash.update(r.as_bytes());
        hash.update(key.verification_key().as_bytes());
        hash.update(message);
        let challenge = Scalar::from_bytes_mod_order_wide(&hash.finalize().into());
        let s = nonce + challenge * secret;
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(r.as_bytes());
        bytes[32..].copy_from_slice(s.as_bytes());
        let cofactored = Signature::new(bytes).unwrap().unwrap();
        assert!(
            tendermint::crypto::default::signature::Verifier::verify(public, message, &cofactored)
                .is_ok()
        );
        assert!(GoEd25519::verify(public, message, &cofactored).is_err());
    }

    #[test]
    fn verifies_nil_votes_and_requires_strict_quorum() {
        let chain: chain::Id = "test-chain".parse().unwrap();
        let height = Height::from(7u32);
        let id = block::Id {
            hash: Hash::Sha256([1; 32]),
            part_set_header: block::parts::Header::new(1, Hash::Sha256([2; 32])).unwrap(),
        };
        let keys = [4u32, 3, 2].map(|power| {
            let key = SigningKey::from([power as u8; 32]);
            let public = PublicKey::from_raw_ed25519(key.verification_key().as_bytes()).unwrap();
            (Info::new(public, power.into()), key)
        });
        let validators = Set::without_proposer(keys.iter().map(|(info, _)| info.clone()).collect());
        let timestamp = Time::unix_epoch();
        let signatures = keys
            .iter()
            .enumerate()
            .map(|(index, (info, key))| {
                let vote = Vote {
                    vote_type: Type::Precommit,
                    height,
                    round: Default::default(),
                    block_id: (index != 2).then_some(id),
                    timestamp: Some(timestamp),
                    validator_address: info.address,
                    validator_index: index.try_into().unwrap(),
                    signature: None,
                    extension: Vec::new(),
                    extension_signature: None,
                };
                let signature =
                    Signature::new(key.sign(&vote.into_signable_vec(chain.clone())).to_bytes())
                        .unwrap();
                if index == 2 {
                    CommitSig::BlockIdFlagNil {
                        validator_address: info.address,
                        timestamp,
                        signature,
                    }
                } else {
                    CommitSig::BlockIdFlagCommit {
                        validator_address: info.address,
                        timestamp,
                        signature,
                    }
                }
            })
            .collect();
        let mut commit = Commit {
            height,
            round: Default::default(),
            block_id: id,
            signatures,
        };
        assert!(verify_commit(&chain, height, id, &validators, &commit).is_ok());
        commit.signatures.swap(0, 1);
        assert!(verify_commit(&chain, height, id, &validators, &commit).is_err());
        commit.signatures.swap(0, 1);
        if let CommitSig::BlockIdFlagNil { signature, .. } = &mut commit.signatures[2] {
            *signature = Signature::new([0; 64]).unwrap();
        }
        assert!(verify_commit(&chain, height, id, &validators, &commit).is_err());

        // A single signer with power 4 of 6 has exactly two thirds.
        let two = Set::without_proposer(vec![keys[0].0.clone(), keys[2].0.clone()]);
        commit.signatures.truncate(2);
        commit.signatures[1] = CommitSig::BlockIdFlagAbsent;
        let error = verify_commit(&chain, height, id, &two, &commit).unwrap_err();
        assert!(error.to_string().contains("insufficient voting power"));
    }
}
