use crate::{Chain, Upgrade, reward_price::Price, state::Amount};
use anyhow::{Context, Result, ensure};
use minter_state::{BlockStore, Checkpoint};
use serde::Deserialize;

pub(crate) struct Identity {
    pub chain_id: tendermint::chain::Id,
    pub initial_height: u64,
    pub transaction_chain_id: u8,
    pub genesis_sha256: Option<[u8; 32]>,
}

impl Identity {
    pub fn validate_snapshot(&self, checkpoint: &Checkpoint) -> Result<()> {
        ensure!(
            checkpoint.network == self.chain_id.as_str(),
            "checkpoint network differs from genesis"
        );
        ensure!(
            checkpoint.format == 1 && checkpoint.height >= self.initial_height,
            "checkpoint height or format differs from genesis"
        );
        Ok(())
    }
}

pub(crate) fn validate_network(checkpoint: &Checkpoint, network: u8) -> Result<()> {
    ensure!(
        checkpoint.network == "minter-mainnet-5" && network == 1,
        "unsupported checkpoint network"
    );
    Ok(())
}

pub(crate) fn restore(
    checkpoint: &Checkpoint,
    network: u8,
    metadata: impl Fn(&str) -> Result<Option<Vec<u8>>>,
) -> Result<Chain> {
    validate_network(checkpoint, network)?;
    restore_metadata(checkpoint, network, metadata)
}

pub(crate) fn restore_for_identity(
    checkpoint: &Checkpoint,
    identity: &Identity,
    metadata: impl Fn(&str) -> Result<Option<Vec<u8>>>,
) -> Result<Chain> {
    identity.validate_snapshot(checkpoint)?;
    let chain = restore_metadata(checkpoint, identity.transaction_chain_id, metadata)?;
    ensure!(
        chain.initial_height == identity.initial_height,
        "AppDB initial height differs from genesis"
    );
    Ok(chain)
}

fn restore_metadata(
    checkpoint: &Checkpoint,
    network: u8,
    metadata: impl Fn(&str) -> Result<Option<Vec<u8>>>,
) -> Result<Chain> {
    let read = |name| -> Result<Vec<u8>> {
        metadata(name)?.with_context(|| format!("missing Go AppDB {name}"))
    };
    let start = u64::from_be_bytes(read("startHeight")?.as_slice().try_into()?);
    ensure!(start < checkpoint.height, "invalid Go AppDB start height");

    let block_times = serde_json::from_slice::<Vec<String>>(&read("blockDelta")?)?
        .into_iter()
        .map(|time| time.parse())
        .collect::<Result<Vec<i64>, _>>()?;
    ensure!(block_times.len() <= 4, "invalid Go block time window");
    let emission = Amount::from_bytes_be(&read("emission")?);
    Ok(Chain {
        id: checkpoint.network.clone(),
        transaction_chain_id: network,
        initial_height: start + 1,
        genesis_hash: [0; 32],
        initial_validators: Vec::new(),
        emission,
        reward_price: decode_price(metadata("price")?.as_deref())?,
        legacy_reward_price_time: None,
        block_times,
        versions: decode_versions(&read("versions")?, checkpoint.height)?,
        retained_candidate_rows: Vec::new(),
        frozen_references: Default::default(),
        native_sync: None,
    })
}

pub(crate) fn reward_price(store: &BlockStore) -> Result<Option<Price>> {
    decode_price(store.checkpoint_metadata("price")?.as_deref())
}

fn decode_price(raw: Option<&[u8]>) -> Result<Option<Price>> {
    raw.map(Price::decode).transpose()
}

pub(crate) fn versions(store: &BlockStore, height: u64) -> Result<Vec<Upgrade>> {
    let raw = store
        .checkpoint_metadata("versions")?
        .context("missing Go upgrade history")?;
    decode_versions(&raw, height)
}

fn decode_versions(raw: &[u8], height: u64) -> Result<Vec<Upgrade>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Version {
        name: String,
        height: String,
    }
    let entries: Vec<Version> = serde_json::from_slice(raw)?;
    ensure!(!entries.is_empty(), "missing Go upgrade history");
    let mut versions = Vec::with_capacity(entries.len());
    let mut previous = 0;
    for entry in entries {
        let activation: u64 = entry.height.parse()?;
        ensure!(
            (previous..=height).contains(&activation) && !entry.name.is_empty(),
            "invalid upgrade {} at {activation}",
            entry.name,
        );
        previous = activation;
        versions.push(Upgrade {
            name: entry.name,
            height: activation,
        });
    }
    Ok(versions)
}

#[cfg(test)]
mod checkpoint_genesis_tests {
    use super::*;

    #[test]
    fn checkpoint_genesis_checks_appdb_height_and_preserves_legacy_parser() {
        let identity = Identity {
            chain_id: "local-checkpoint".parse().unwrap(),
            initial_height: 101,
            transaction_chain_id: 1,
            genesis_sha256: Some([7; 32]),
        };
        let mut snapshot = Checkpoint {
            format: 1,
            network: identity.chain_id.to_string(),
            height: 110,
            block_hash: [1; 32],
            app_hash: [2; 32],
            snapshot_sha256: [3; 32],
            bytes: 1,
            nodes: 1,
        };
        let read = |start: u64, name: &str| -> Result<Option<Vec<u8>>> {
            Ok(match name {
                "startHeight" => Some(start.to_be_bytes().to_vec()),
                "blockDelta" => Some(b"[]".to_vec()),
                "emission" => Some(vec![1]),
                "versions" => Some(br#"[{"Name":"v300","Height":"0"}]"#.to_vec()),
                _ => None,
            })
        };
        assert_eq!(
            restore_for_identity(&snapshot, &identity, |name| read(100, name))
                .unwrap()
                .initial_height,
            101
        );
        assert!(
            restore_for_identity(&snapshot, &identity, |name| read(99, name))
                .err()
                .unwrap()
                .to_string()
                .contains("AppDB initial height")
        );
        snapshot.network = "minter-mainnet-5".into();
        assert_eq!(
            restore(&snapshot, 1, |name| read(99, name))
                .unwrap()
                .initial_height,
            100
        );
    }
}
