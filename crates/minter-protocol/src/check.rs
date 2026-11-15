//! Minter check fields, issuer signature, and proof of the recipient's lock.

use crate::{
    Address, DecodeError, InvalidSignature, Signature, SigningHash, full_list, hash_rlp_list,
    integer_bytes,
};
use alloy_rlp::{Decodable, Encodable, Header};
use k256::ecdsa::{RecoveryId, Signature as K256Signature, VerifyingKey};
use thiserror::Error;

#[derive(Debug)]
pub struct Check<'a> {
    pub nonce: &'a [u8],
    pub chain_id: u8,
    pub due_block: u64,
    pub coin: u32,
    pub value: &'a [u8],
    pub gas_coin: u32,
    lock: &'a [u8],
    signature: Signature<'a>,
    hash: SigningHash,
    lock_hash: SigningHash,
}

#[derive(Debug, Error)]
pub enum ProofError {
    #[error("invalid signature length")]
    Length,
    #[error("invalid signature recovery id")]
    RecoveryId,
    #[error("recovery failed")]
    Recovery,
}

impl<'a> Check<'a> {
    pub fn decode(raw: &'a [u8]) -> Result<Self, DecodeError> {
        let body = full_list(raw)?;
        let mut fields = body;
        let nonce = Header::decode_bytes(&mut fields, false)?;
        let chain_id = u8::decode(&mut fields)?;
        let due_block = u64::decode(&mut fields)?;
        let coin = u32::decode(&mut fields)?;
        let value = integer_bytes(&mut fields)?;
        let gas_coin = u32::decode(&mut fields)?;
        let lock_hash = hash_rlp_list(&body[..body.len() - fields.len()]);
        let lock = integer_bytes(&mut fields)?;
        let hash = hash_rlp_list(&body[..body.len() - fields.len()]);
        let signature = Signature::from_list(fields)?;
        Ok(Self {
            nonce,
            chain_id,
            due_block,
            coin,
            value,
            gas_coin,
            lock,
            signature,
            hash,
            lock_hash,
        })
    }

    /// Go marks the hash of the signed fields as redeemed, without V/R/S.
    pub fn hash(&self) -> SigningHash {
        self.hash
    }

    pub fn issuer(&self) -> Result<Address, InvalidSignature> {
        self.signature.recover(&self.hash)
    }

    pub fn verify_recipient(
        &self,
        recipient: Address,
        proof: &[u8; 65],
    ) -> Result<bool, ProofError> {
        let mut lock = [0; 65];
        let offset = lock
            .len()
            .checked_sub(self.lock.len())
            .ok_or(ProofError::Length)?;
        lock[offset..].copy_from_slice(self.lock);
        let lock_key = recover_key(&self.lock_hash, &lock)?;
        let mut body = Vec::new();
        recipient.encode(&mut body);
        let recipient_hash = hash_rlp_list(&body);
        Ok(recover_key(&recipient_hash, proof)? == lock_key)
    }
}

fn recover_key(hash: &SigningHash, compact: &[u8; 65]) -> Result<VerifyingKey, ProofError> {
    let id = RecoveryId::from_byte(compact[64]).ok_or(ProofError::RecoveryId)?;
    let signature = K256Signature::from_slice(&compact[..64]).map_err(|_| ProofError::Recovery)?;
    // Recovering a lock accepts both S values. Normalizing S changes
    // the recovery point's parity, so flip that bit for the same key.
    let normalized = signature.normalize_s();
    let id = RecoveryId::new(id.is_y_odd() ^ (signature != normalized), id.is_x_reduced());
    VerifyingKey::recover_from_prehash(hash, &normalized, id).map_err(|_| ProofError::Recovery)
}
