use anyhow::{Context, Result, ensure};
use minter_protocol::{Authorization, Envelope};
use serde::{Deserialize, Serialize};
use std::io::BufRead;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoEnvelope {
    height: u64,
    index: usize,
    #[serde(with = "hex::serde")]
    raw: Vec<u8>,
    nonce: u64,
    chain_id: u8,
    gas_price: u32,
    gas_coin: u32,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(with = "hex::serde")]
    data: Vec<u8>,
    #[serde(with = "hex::serde")]
    payload: Vec<u8>,
    #[serde(with = "hex::serde")]
    service_data: Vec<u8>,
    signature_type: u8,
    #[serde(with = "hex::serde")]
    signature_data: Vec<u8>,
    #[serde(with = "hex::serde")]
    signing_hash: [u8; 32],
    #[serde(with = "hex::serde")]
    sender: [u8; 20],
    #[serde(default)]
    signers: Vec<Option<String>>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub single: usize,
    pub multisig: usize,
    pub multisig_signatures: usize,
}

pub fn compare(source: impl BufRead) -> Result<Counts> {
    let mut counts = Counts::default();
    for (line_number, line) in source.lines().enumerate() {
        let go: GoEnvelope = serde_json::from_str(&line?)
            .with_context(|| format!("Go reference line {}", line_number + 1))?;
        let tx = Envelope::decode(&go.raw)
            .with_context(|| format!("decode transaction {}:{}", go.height, go.index))?;
        let fields = tx.fields();
        let (kind, sender) = match tx.authorization() {
            Authorization::Single(signature) => {
                counts.single += 1;
                (1, signature.recover(tx.signing_hash())?)
            }
            Authorization::Multisig {
                account,
                signatures,
            } => {
                ensure!(
                    signatures.len() == go.signers.len(),
                    "multisig signature count differs from Go at {}:{}",
                    go.height,
                    go.index,
                );
                for (signature, expected) in signatures.iter().zip(&go.signers) {
                    let actual = signature.recover(tx.signing_hash()).ok().map(hex::encode);
                    ensure!(
                        &actual == expected,
                        "multisig signer differs from Go at {}:{}",
                        go.height,
                        go.index,
                    );
                }
                counts.multisig += 1;
                counts.multisig_signatures += signatures.len();
                (2, *account)
            }
        };
        ensure!(
            fields.nonce == go.nonce
                && fields.chain_id == go.chain_id
                && fields.gas_price == go.gas_price
                && fields.gas_coin == go.gas_coin
                && fields.kind == go.kind
                && fields.data == go.data
                && fields.payload == go.payload
                && fields.service_data == go.service_data
                && kind == go.signature_type
                && tx.signature_data() == go.signature_data
                && tx.signing_hash() == &go.signing_hash
                && sender == go.sender,
            "envelope differs from Go at {}:{}",
            go.height,
            go.index,
        );
    }
    Ok(counts)
}
