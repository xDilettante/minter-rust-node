//! Wire types for the Minter protocol.

use alloy_rlp::{Decodable, Encodable, Header};
use k256::ecdsa::{RecoveryId, Signature as K256Signature, VerifyingKey};
use sha3::{Digest, Keccak256};
use thiserror::Error;

pub type Address = [u8; 20];
pub mod check;
pub mod state_keys;
pub type SigningHash = [u8; 32];
pub const MAX_TX_BYTES: usize = 16_144;

/// Address of the multisig account created by a Minter transaction.
pub fn multisig_address(creator: Address, nonce: u64) -> Address {
    let mut body = Vec::new();
    creator.encode(&mut body);
    nonce.encode(&mut body);
    hash_rlp_list(&body)[12..]
        .try_into()
        .expect("20-byte address")
}

fn hash_rlp_list(body: &[u8]) -> SigningHash {
    let mut header = Vec::new();
    Header {
        list: true,
        payload_length: body.len(),
    }
    .encode(&mut header);
    Keccak256::new()
        .chain_update(header)
        .chain_update(body)
        .finalize()
        .into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fields<'a> {
    pub nonce: u64,
    pub chain_id: u8,
    pub gas_price: u32,
    pub gas_coin: u32,
    pub kind: u8,
    pub data: &'a [u8],
    pub payload: &'a [u8],
    pub service_data: &'a [u8],
}

#[derive(Debug)]
pub struct Envelope<'a> {
    fields: Fields<'a>,
    authorization: Authorization<'a>,
    signature_data: &'a [u8],
    signing_hash: SigningHash,
}

#[derive(Debug)]
pub enum Authorization<'a> {
    Single(Signature<'a>),
    /// The executor must check the account's authorized signers and weights.
    Multisig {
        account: Address,
        signatures: Vec<Signature<'a>>,
    },
}

#[derive(Debug)]
pub struct Signature<'a> {
    v: &'a [u8],
    r: &'a [u8],
    s: &'a [u8],
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("transaction exceeds {MAX_TX_BYTES} bytes")]
    TooLarge,
    #[error("invalid RLP")]
    Rlp(#[from] alloy_rlp::Error),
    #[error("unexpected trailing RLP bytes")]
    TrailingBytes,
    #[error("unknown signature type {0}")]
    UnknownSignatureType(u8),
    #[error("multisig account must be 20 bytes")]
    AddressLength,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum InvalidSignature {
    #[error("signature recovery id must be 27 or 28")]
    RecoveryId,
    #[error("invalid secp256k1 signature scalar")]
    Scalar,
    #[error("signature S is above half the curve order")]
    HighS,
    #[error("could not recover secp256k1 public key")]
    Recovery,
}

impl<'a> Envelope<'a> {
    /// Decode canonical RLP for a bounded Minter transaction. Execution rules
    /// interpret the transaction kind and its data at the block's version.
    pub fn decode(raw: &'a [u8]) -> Result<Self, DecodeError> {
        if raw.len() > MAX_TX_BYTES {
            return Err(DecodeError::TooLarge);
        }
        let body = full_list(raw)?;
        let mut rest = body;
        let fields = Fields {
            nonce: u64::decode(&mut rest)?,
            chain_id: u8::decode(&mut rest)?,
            gas_price: u32::decode(&mut rest)?,
            gas_coin: u32::decode(&mut rest)?,
            kind: u8::decode(&mut rest)?,
            data: Header::decode_bytes(&mut rest, false)?,
            payload: Header::decode_bytes(&mut rest, false)?,
            service_data: Header::decode_bytes(&mut rest, false)?,
        };
        let signature_type = u8::decode(&mut rest)?;
        let signed_body = &body[..body.len() - rest.len()];
        let signature_data = Header::decode_bytes(&mut rest, false)?;
        finish(rest)?;
        let authorization = match signature_type {
            1 => Authorization::Single(Signature::from_list(full_list(signature_data)?)?),
            2 => {
                let mut fields = full_list(signature_data)?;
                let account = Header::decode_bytes(&mut fields, false)?
                    .try_into()
                    .map_err(|_| DecodeError::AddressLength)?;
                let mut remaining_signatures = Header::decode_bytes(&mut fields, true)?;
                finish(fields)?;
                let mut signatures = Vec::new();
                while !remaining_signatures.is_empty() {
                    let signature = Header::decode_bytes(&mut remaining_signatures, true)?;
                    signatures.push(Signature::from_list(signature)?);
                }
                Authorization::Multisig {
                    account,
                    signatures,
                }
            }
            other => return Err(DecodeError::UnknownSignatureType(other)),
        };
        // Canonical fields can be hashed directly under the nine-field list header.
        let signing_hash = hash_rlp_list(signed_body);
        Ok(Self {
            fields,
            authorization,
            signature_data,
            signing_hash,
        })
    }

    pub fn fields(&self) -> Fields<'a> {
        self.fields
    }

    pub fn authorization(&self) -> &Authorization<'a> {
        &self.authorization
    }

    pub fn into_authorization(self) -> Authorization<'a> {
        self.authorization
    }

    pub fn signature_data(&self) -> &'a [u8] {
        self.signature_data
    }

    pub fn signing_hash(&self) -> &SigningHash {
        &self.signing_hash
    }
}

impl<'a> Signature<'a> {
    fn from_list(mut body: &'a [u8]) -> Result<Self, DecodeError> {
        let signature = Self {
            v: integer_bytes(&mut body)?,
            r: integer_bytes(&mut body)?,
            s: integer_bytes(&mut body)?,
        };
        finish(body)?;
        Ok(signature)
    }

    pub fn recover(&self, hash: &SigningHash) -> Result<Address, InvalidSignature> {
        let odd = match self.v {
            [27] => false,
            [28] => true,
            _ => return Err(InvalidSignature::RecoveryId),
        };
        let signature = K256Signature::from_scalars(scalar(self.r)?, scalar(self.s)?)
            .map_err(|_| InvalidSignature::Scalar)?;
        if signature.normalize_s() != signature {
            return Err(InvalidSignature::HighS);
        }
        let key = VerifyingKey::recover_from_prehash(hash, &signature, RecoveryId::new(odd, false))
            .map_err(|_| InvalidSignature::Recovery)?;
        let point = key.to_sec1_point(false);
        let digest = Keccak256::digest(&point.as_bytes()[1..]);
        let mut address = [0; 20];
        address.copy_from_slice(&digest[12..]);
        Ok(address)
    }
}

fn full_list(mut input: &[u8]) -> Result<&[u8], DecodeError> {
    let payload = Header::decode_bytes(&mut input, true)?;
    finish(input)?;
    Ok(payload)
}

fn finish(input: &[u8]) -> Result<(), DecodeError> {
    if input.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::TrailingBytes)
    }
}

fn integer_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], DecodeError> {
    let bytes = Header::decode_bytes(input, false)?;
    if bytes.first() == Some(&0) {
        return Err(alloy_rlp::Error::LeadingZero.into());
    }
    Ok(bytes)
}

fn scalar(bytes: &[u8]) -> Result<[u8; 32], InvalidSignature> {
    let mut scalar = [0; 32];
    let offset = scalar
        .len()
        .checked_sub(bytes.len())
        .ok_or(InvalidSignature::Scalar)?;
    scalar[offset..].copy_from_slice(bytes);
    Ok(scalar)
}
