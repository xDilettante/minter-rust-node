mod rpc;
mod tx;

use alloy_rlp::{Decodable, Header};
use anyhow::{Context, Result, ensure};
use minter_protocol::{
    Address,
    state_keys::{account_key, balance_key},
};
use num_bigint::BigUint;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use rpc::{Peer, Rpc};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
use tendermint::{Hash, Time, block::Height, chain, trust_threshold::TrustThresholdFraction};
use tendermint_light_client::{
    builder::LightClientBuilder,
    components::{clock::SystemClock, scheduler::basic_bisecting_schedule},
    instance::Instance,
    light_client::Options,
    predicates::ProdPredicates,
    store::memory::MemoryStore,
    types::LightBlock,
    verifier::{ProdVerifier, Verdict},
};

const CONFIG: TableDefinition<&str, &[u8]> = TableDefinition::new("config");
const HEADERS: TableDefinition<u64, &[u8]> = TableDefinition::new("verified_headers");
const ANCHOR_KEY: &str = "anchor";
const RETAINED_HEADERS: u64 = 256;
pub use tx::Transaction;

/// A locally authenticated Tendermint checkpoint and the parameters of its chain.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustAnchor {
    pub chain_id: chain::Id,
    pub height: Height,
    pub hash: Hash,
    pub initial_height: Height,
    pub trusting_period_secs: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub chain_id: chain::Id,
    pub height: u64,
    pub hash: Hash,
    pub app_hash: String,
    pub time: Time,
    pub trust_expires_at: Time,
    pub stale: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub chain_id: chain::Id,
    pub height: u64,
    pub header_height: u64,
    pub app_hash: String,
    pub address: String,
    pub coin: u32,
    pub balance: String,
    pub nonce: u64,
    pub stale: bool,
}

pub struct LightClient {
    db: Database,
    anchor: TrustAnchor,
}

impl LightClient {
    pub fn initialize(
        path: &Path,
        anchor: TrustAnchor,
        primary: &str,
        witness: &str,
    ) -> Result<Status> {
        ensure!(
            anchor.initial_height.value() > 0
                && anchor.height >= anchor.initial_height
                && anchor.hash.as_bytes().len() == 32
                && anchor.trusting_period_secs > 0,
            "invalid Tendermint trust anchor"
        );
        let (primary, witness) = peers(&anchor, primary, witness)?;
        let trusted = check_anchor(primary, &anchor)?;
        let other = check_anchor(witness, &anchor)?;
        same_header(&trusted, &other)?;

        let db = Database::create(path)?;
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        {
            let mut config = tx.open_table(CONFIG)?;
            ensure!(
                config.get(ANCHOR_KEY)?.is_none(),
                "light database already has a trust anchor"
            );
            config.insert(ANCHOR_KEY, serde_json::to_vec(&anchor)?.as_slice())?;
            let mut headers = tx.open_table(HEADERS)?;
            headers.insert(
                anchor.height.value(),
                serde_json::to_vec(&trusted)?.as_slice(),
            )?;
        }
        tx.commit()?;
        describe(&anchor, &trusted)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::open(path).context("open light client redb database")?;
        let anchor = {
            let tx = db.begin_read()?;
            let config = tx.open_table(CONFIG)?;
            let raw = config
                .get(ANCHOR_KEY)?
                .context("missing light client trust anchor")?;
            serde_json::from_slice(raw.value())?
        };
        Ok(Self { db, anchor })
    }

    pub fn status(&self) -> Result<Status> {
        describe(&self.anchor, &self.header(None)?)
    }

    pub fn sync(&self, primary: &str, witness: &str, height: Option<u64>) -> Result<Status> {
        let current = self.header(None)?;
        let (primary, witness) = peers(&self.anchor, primary, witness)?;
        // A light block also needs the validator set for its successor.
        let latest = primary.latest.min(witness.latest).value().saturating_sub(1);
        let target =
            Height::try_from(height.unwrap_or_else(|| latest.max(current.height().value())))?;
        let trusted = if target >= current.height() {
            current.clone()
        } else {
            self.header_at_or_before(target.value())?
        };
        if target == trusted.height() {
            // Revalidate time even when the available RPCs have no newer height.
            instance(primary, &self.anchor, trusted.clone())?;
            return describe(&self.anchor, &trusted);
        }
        ensure!(
            target.value() <= latest,
            "RPC peers have not reached the requested height and its successor"
        );
        let next = advance(primary, &self.anchor, &trusted, target)?;
        let other = advance(witness, &self.anchor, &trusted, target)?;
        same_header(&next, &other)?;

        let mut tx = self.db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        {
            let mut headers = tx.open_table(HEADERS)?;
            let persisted = headers
                .last()?
                .context("missing verified header")?
                .0
                .value();
            ensure!(
                persisted == current.height().value(),
                "verified head advanced during sync, retry"
            );
            if let Some(stored) = headers.get(target.value())? {
                same_header(&self.decode_header(target.value(), stored.value())?, &next)?;
            }
            if let Some(raw) = headers.get(target.value() - 1)? {
                let prior = self.decode_header(target.value() - 1, raw.value())?;
                linked_headers(&prior, &next)?;
            }
            if let Some(raw) = headers.get(target.value() + 1)? {
                let successor = self.decode_header(target.value() + 1, raw.value())?;
                linked_headers(&next, &successor)?;
            }
            headers.insert(target.value(), serde_json::to_vec(&next)?.as_slice())?;
            if headers.len()? > RETAINED_HEADERS {
                headers.pop_first()?;
            }
        }
        tx.commit()?;
        describe(&self.anchor, &next)
    }

    pub fn balance(
        &self,
        endpoint: &str,
        address: Address,
        coin: u32,
        state_height: Option<u64>,
    ) -> Result<Balance> {
        let header_height = state_height
            .map(|height| height.checked_add(1).context("state height overflow"))
            .transpose()?;
        let header = self.header(header_height)?;
        let status = describe(&self.anchor, &header)?;
        let height = status
            .height
            .checked_sub(1)
            .context("missing state height")?;
        ensure!(
            height > 0,
            "query requires a committed positive state height"
        );
        let root = header
            .signed_header
            .header
            .app_hash
            .as_bytes()
            .try_into()
            .context("verified header does not contain a Minter IAVL root")?;
        let version = height + u64::from(self.anchor.initial_height.value() == 1);
        let rpc = Rpc::new(endpoint)?;
        rpc.clone()
            .peer(&self.anchor.chain_id)
            .context("proof RPC")?;
        let value_at = |key: &[u8]| -> Result<Option<Vec<u8>>> {
            let reply = rpc.iavl(height, key)?;
            ensure!(reply.code.is_ok(), "IAVL proof query failed: {}", reply.log);
            ensure!(
                reply.height.value() == height && reply.key == key,
                "RPC returned another proof height or key"
            );
            let ops = reply.proof.context("RPC returned no IAVL proof")?.ops;
            let [op] = ops.as_slice() else {
                anyhow::bail!("expected one IAVL proof operation");
            };
            ensure!(
                op.field_type == minter_iavl::PROOF_OP_TYPE && op.key == key,
                "invalid IAVL proof operation"
            );
            let value = minter_iavl::verify_proof(root, version, key, &op.data)?;
            ensure!(
                reply.value == value.as_deref().unwrap_or_default(),
                "RPC value differs from IAVL proof"
            );
            Ok(value)
        };
        let (balance, nonce) = proved_balance(address, coin, value_at)?;
        Ok(Balance {
            chain_id: status.chain_id,
            height,
            header_height: status.height,
            app_hash: status.app_hash,
            address: format!("Mx{}", hex::encode(address)),
            coin,
            balance: balance.to_string(),
            nonce,
            stale: status.stale,
        })
    }

    pub fn transaction(
        &self,
        endpoint: &str,
        hash: [u8; 32],
        height: Option<u64>,
        verify_result: bool,
    ) -> Result<Transaction> {
        let rpc = Rpc::new(endpoint)?;
        rpc.clone()
            .peer(&self.anchor.chain_id)
            .context("transaction RPC")?;
        let reply = rpc.transaction(hash)?;
        ensure!(
            height.is_none_or(|expected| expected == reply.height.value()),
            "RPC returned another transaction height"
        );
        let block = self.header(Some(reply.height.value()))?;
        let results = if verify_result {
            let successor = self.header(Some(reply.height.value() + 1))?;
            linked_headers(&block, &successor)?;
            let root = successor
                .signed_header
                .header
                .last_results_hash
                .context("verified successor has no results root")?;
            Some((root, rpc.results(reply.height)?))
        } else {
            None
        };
        tx::verify(
            describe(&self.anchor, &block)?,
            &block.signed_header.header,
            hash,
            reply,
            results,
        )
    }

    fn header(&self, height: Option<u64>) -> Result<LightBlock> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(HEADERS)?;
        let (height, raw) = match height {
            Some(height) => (
                height,
                table
                    .get(height)?
                    .context("requested signed header is not in the verified cache")?,
            ),
            None => {
                let (height, raw) = table.last()?.context("missing verified header")?;
                (height.value(), raw)
            }
        };
        self.decode_header(height, raw.value())
    }

    fn header_at_or_before(&self, height: u64) -> Result<LightBlock> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(HEADERS)?;
        let (height, raw) = table
            .range(..=height)?
            .next_back()
            .transpose()?
            .context("no cached trust anchor at or before the requested height")?;
        self.decode_header(height.value(), raw.value())
    }

    fn decode_header(&self, height: u64, raw: &[u8]) -> Result<LightBlock> {
        let block: LightBlock = serde_json::from_slice(raw)?;
        ensure!(
            block.height().value() == height
                && block.signed_header.header.chain_id == self.anchor.chain_id,
            "invalid cached Tendermint header"
        );
        Ok(block)
    }
}

fn peers(anchor: &TrustAnchor, primary: &str, witness: &str) -> Result<(Peer, Peer)> {
    let primary = Rpc::new(primary)?;
    let witness = Rpc::new(witness)?;
    ensure!(
        !primary.same_endpoint(&witness),
        "primary and witness must be distinct Tendermint nodes"
    );
    let primary = primary.peer(&anchor.chain_id).context("primary RPC")?;
    let witness = witness.peer(&anchor.chain_id).context("witness RPC")?;
    ensure!(
        primary.id != witness.id,
        "primary and witness must be distinct Tendermint nodes"
    );
    Ok((primary, witness))
}

fn options(anchor: &TrustAnchor) -> Options {
    Options {
        trust_threshold: TrustThresholdFraction::ONE_THIRD,
        trusting_period: Duration::from_secs(anchor.trusting_period_secs),
        clock_drift: Duration::from_secs(5),
    }
}

fn check_anchor(peer: Peer, anchor: &TrustAnchor) -> Result<LightBlock> {
    let block = peer.light_block(anchor.height)?;
    ensure!(
        block.signed_header.header.hash() == anchor.hash,
        "RPC disagrees with the trusted header hash"
    );
    let verifier = ProdVerifier::default();
    let untrusted = block.as_untrusted_state();
    let sets = verifier.verify_validator_sets(&untrusted);
    ensure!(
        sets == Verdict::Success,
        "invalid trust anchor validator sets or commit: {sets:?}"
    );
    let commit = verifier.verify_commit(&untrusted);
    ensure!(
        commit == Verdict::Success,
        "invalid trust anchor signatures: {commit:?}"
    );
    instance(peer, anchor, block.clone())?;
    Ok(block)
}

fn instance(peer: Peer, anchor: &TrustAnchor, trusted: LightBlock) -> Result<Instance> {
    let client = LightClientBuilder::custom(
        peer.id,
        options(anchor),
        Box::new(MemoryStore::new()),
        Box::new(peer),
        Box::new(SystemClock),
        Box::new(ProdVerifier::default()),
        Box::new(basic_bisecting_schedule),
        Box::new(ProdPredicates),
    )
    .trust_light_block(trusted)
    .context("invalid or expired trusted header")?
    .build();
    Ok(client)
}

fn advance(
    peer: Peer,
    anchor: &TrustAnchor,
    trusted: &LightBlock,
    target: Height,
) -> Result<LightBlock> {
    let mut client = instance(peer, anchor, trusted.clone())?;
    client
        .light_client
        .verify_to_target(target, &mut client.state)
        .context("Tendermint light verification failed")
}

fn same_header(first: &LightBlock, second: &LightBlock) -> Result<()> {
    ensure!(
        first.signed_header.header.hash() == second.signed_header.header.hash(),
        "primary RPC and witness report different signed headers"
    );
    Ok(())
}

fn linked_headers(first: &LightBlock, second: &LightBlock) -> Result<()> {
    ensure!(
        second
            .signed_header
            .header
            .last_block_id
            .as_ref()
            .map(|id| id.hash)
            == Some(first.signed_header.header.hash()),
        "adjacent signed header conflicts with the verified cache"
    );
    Ok(())
}

fn describe(anchor: &TrustAnchor, block: &LightBlock) -> Result<Status> {
    let header = &block.signed_header.header;
    let options = options(anchor);
    let expires = header
        .time
        .checked_add(options.trusting_period)
        .context("trust expiration overflow")?;
    let now = Time::now();
    Ok(Status {
        chain_id: header.chain_id.clone(),
        height: header.height.value(),
        hash: header.hash(),
        app_hash: hex::encode(header.app_hash.as_bytes()),
        time: header.time,
        trust_expires_at: expires,
        stale: now >= expires
            || now
                .checked_add(options.clock_drift)
                .is_none_or(|time| header.time >= time),
    })
}

fn proved_balance(
    address: Address,
    coin: u32,
    read: impl Fn(&[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<(BigUint, u64)> {
    let Some(model) = read(&account_key(address, b""))?.filter(|value| !value.is_empty()) else {
        return Ok((BigUint::ZERO, 0));
    };
    let mut fields = rlp_list(&model)?;
    let nonce = u64::decode(&mut fields)?;
    let _multisig = Header::decode_bytes(&mut fields, true)?;
    let _lock = u64::decode(&mut fields)?;
    ensure!(fields.is_empty(), "trailing account fields");
    let Some(coins) = read(&account_key(address, b"c"))?.filter(|value| !value.is_empty()) else {
        return Ok((BigUint::ZERO, nonce));
    };
    let mut ids = rlp_list(&coins)?;
    let mut listed = false;
    while !ids.is_empty() {
        listed |= u32::decode(&mut ids)? == coin;
    }
    let balance = if listed {
        read(&balance_key(address, coin))?
            .map_or(BigUint::ZERO, |value| BigUint::from_bytes_be(&value))
    } else {
        BigUint::ZERO
    };
    Ok((balance, nonce))
}

fn rlp_list(mut input: &[u8]) -> Result<&[u8]> {
    let list = Header::decode_bytes(&mut input, true)?;
    ensure!(input.is_empty(), "trailing account RLP bytes");
    Ok(list)
}
