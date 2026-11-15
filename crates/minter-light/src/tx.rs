use crate::Status;
use anyhow::{Context, Result, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};
use tendermint::{
    Hash,
    block::{Header, Height},
    chain,
    crypto::{Sha256, default::Sha256 as Hasher},
    merkle::MerkleHash,
};
use tendermint_proto::v0_34::abci::ResponseDeliverTx;
use tendermint_rpc::endpoint::tx::v0_34::DialectResponse;

// Minter emits partial consensus parameter updates. Only the DeliverTx
// fields committed by LastResultsHash are needed from this endpoint.
#[derive(Deserialize)]
pub(crate) struct Results {
    height: Height,
    txs_results: Option<Vec<DeliverResult>>,
}

impl tendermint_rpc::Response for Results {}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeliverResult {
    code: u32,
    #[serde(with = "tendermint_proto::serializers::bytes::base64string")]
    data: Vec<u8>,
    #[serde(with = "tendermint_proto::serializers::from_str")]
    gas_wanted: i64,
    #[serde(with = "tendermint_proto::serializers::from_str")]
    gas_used: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub chain_id: chain::Id,
    pub height: u64,
    pub hash: String,
    pub data_hash: String,
    pub included: bool,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub index: u32,
    pub code: u32,
    pub data: String,
    pub gas_wanted: i64,
    pub gas_used: i64,
    pub results_hash: String,
}

pub(crate) fn verify(
    status: Status,
    header: &Header,
    expected: [u8; 32],
    reply: DialectResponse,
    results: Option<(Hash, Results)>,
) -> Result<Transaction> {
    ensure!(
        reply.hash.as_bytes() == expected && Hasher::digest(&reply.tx) == expected,
        "transaction bytes differ from the requested hash"
    );
    let data_hash = header
        .data_hash
        .context("verified header has no transaction root")?;
    let proof = reply.proof.context("RPC returned no transaction proof")?;
    ensure!(
        proof.data == reply.tx
            && proof.root_hash == data_hash
            && proof.proof.index == u64::from(reply.index)
            && proof.proof.aunts.len() <= 64,
        "transaction proof differs from the verified header or query"
    );

    // Tendermint 0.34 builds the tree over SHA-256 transaction hashes.
    let mut hasher = Hasher::default();
    let leaf = hasher.leaf_hash(&expected);
    ensure!(
        proof.proof.leaf_hash.as_bytes() == leaf,
        "invalid Tendermint transaction leaf"
    );
    let root = merkle_root(
        &mut hasher,
        proof.proof.index,
        proof.proof.total,
        leaf,
        &proof.proof.aunts,
    )?;
    ensure!(
        root == data_hash.as_bytes(),
        "transaction proof root differs from the verified header"
    );
    let execution = results
        .map(|(root, results)| {
            verify_execution(root, status.height, reply.index, proof.proof.total, results)
        })
        .transpose()?;
    Ok(Transaction {
        chain_id: status.chain_id,
        height: status.height,
        hash: hex::encode(expected),
        data_hash: hex::encode(root),
        included: true,
        stale: status.stale,
        execution,
    })
}

fn verify_execution(
    root: Hash,
    height: u64,
    index: u32,
    total: u64,
    response: Results,
) -> Result<Execution> {
    ensure!(
        response.height.value() == height,
        "RPC returned another execution height"
    );
    let results = response
        .txs_results
        .context("RPC returned no execution results")?;
    ensure!(
        results.len() as u64 == total,
        "execution results count differs from the transaction proof"
    );
    // Tendermint 0.34 commits these four deterministic fields, in transaction order.
    let deterministic: Vec<_> = results
        .into_iter()
        .map(|result| ResponseDeliverTx {
            code: result.code,
            data: result.data.into(),
            gas_wanted: result.gas_wanted,
            gas_used: result.gas_used,
            ..Default::default()
        })
        .collect();
    let encoded: Vec<_> = deterministic.iter().map(Message::encode_to_vec).collect();
    let computed = Hasher::default().hash_byte_vectors(&encoded);
    ensure!(
        computed == root.as_bytes(),
        "execution results root differs from the verified successor"
    );
    let result = deterministic
        .get(index as usize)
        .context("transaction index exceeds the verified results")?;
    Ok(Execution {
        index,
        code: result.code,
        data: hex::encode(&result.data),
        gas_wanted: result.gas_wanted,
        gas_used: result.gas_used,
        results_hash: hex::encode(computed),
    })
}

fn merkle_root(
    hash: &mut impl MerkleHash,
    index: u64,
    total: u64,
    leaf: [u8; 32],
    aunts: &[Hash],
) -> Result<[u8; 32]> {
    ensure!(index < total, "invalid transaction proof index or total");
    if total == 1 {
        ensure!(aunts.is_empty(), "transaction proof has extra siblings");
        return Ok(leaf);
    }
    let (sibling, aunts) = aunts
        .split_last()
        .context("transaction proof is missing a sibling")?;
    let sibling = sibling
        .as_bytes()
        .try_into()
        .context("invalid transaction sibling hash")?;
    let split = 1 << (u64::BITS - 1 - (total - 1).leading_zeros());
    if index < split {
        let left = merkle_root(hash, index, split, leaf, aunts)?;
        Ok(hash.inner_hash(left, sibling))
    } else {
        let right = merkle_root(hash, index - split, total - split, leaf, aunts)?;
        Ok(hash.inner_hash(sibling, right))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tendermint_light_client::types::LightBlock;

    #[test]
    fn checks_the_asymmetric_tree_and_exact_sibling_count() {
        let mut hash = Hasher::default();
        let keys = [b"a", b"b", b"c"].map(Hasher::digest);
        let [a, b, c] = keys.map(|key| hash.leaf_hash(&key));
        let right = Hash::Sha256(c);
        let ab = Hash::Sha256(hash.inner_hash(a, b));
        let root = hash.hash_byte_vectors(&keys);
        assert_eq!(merkle_root(&mut hash, 2, 3, c, &[ab]).unwrap(), root);
        assert_eq!(
            merkle_root(&mut hash, 0, 3, a, &[Hash::Sha256(b), right]).unwrap(),
            root
        );
        assert!(merkle_root(&mut hash, 3, 3, c, &[ab]).is_err());
        assert!(merkle_root(&mut hash, 0, 3, a, &[right]).is_err());
        assert!(merkle_root(&mut hash, 2, 3, c, &[right, ab]).is_err());
    }

    #[test]
    fn checks_real_go_execution_and_rejects_an_uncommitted_position() {
        let fixture: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/go-light-blocks.json")).unwrap();
        let at = |height: &str| {
            fixture
                .iter()
                .find(|row| row["signed_header"]["header"]["height"] == height)
                .unwrap()
        };
        let block: LightBlock = serde_json::from_value(at("40561603").clone()).unwrap();
        let successor: LightBlock = serde_json::from_value(at("40561604").clone()).unwrap();
        let root = successor.signed_header.header.last_results_hash.unwrap();
        let queries = &at("40561603")["queries"];
        let check = |reply: DialectResponse, results| {
            let header = &block.signed_header.header;
            let hash = reply.hash.as_bytes().try_into().unwrap();
            let status = Status {
                chain_id: header.chain_id.clone(),
                height: header.height.value(),
                hash: header.hash(),
                app_hash: hex::encode(header.app_hash.as_bytes()),
                time: header.time,
                trust_expires_at: header.time,
                stale: true,
            };
            verify(status, header, hash, reply, results)
        };
        for (index, expected) in [0, 0, 303].into_iter().enumerate() {
            let reply = serde_json::from_value(queries["transactions"][index].clone()).unwrap();
            let results = serde_json::from_value(queries["results"].clone()).unwrap();
            let verified = check(reply, Some((root, results)))
                .unwrap()
                .execution
                .unwrap();
            assert_eq!((verified.index, verified.code), (index as u32, expected));
        }

        let mut reply: DialectResponse =
            serde_json::from_value(queries["transactions"][2].clone()).unwrap();
        reply.index = 4;
        reply.proof.as_mut().unwrap().proof.index = 4;
        reply.proof.as_mut().unwrap().proof.total = 5;
        let results = serde_json::from_value(queries["results"].clone()).unwrap();
        let error = check(reply, Some((root, results))).err().unwrap();
        assert!(error.to_string().contains("execution results count"));
    }
}
