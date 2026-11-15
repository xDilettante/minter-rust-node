pub mod api;
mod bancor;
mod binary;
mod checkpoint;
mod coins;
mod commission;
mod evidence;
mod frozen;
mod genesis;
mod halts;
pub mod native_sync;
mod orders;
mod pool;
mod reward_price;
mod rewards;
mod stakes;
mod state;
mod state_sync;
mod transaction;
mod updates;
pub mod validator_signer;

use anyhow::{Context, Result, ensure};
use commission::Fees;
use minter_api::{
    Any, encode_events, pack,
    pb::{
        JailEvent, RemoveCandidateEvent, StakeMoveEvent, UnbondEvent, UnlockEvent,
        UpdateNetworkEvent, UpdatedBlockRewardEvent,
    },
};
use minter_protocol::{Address, SigningHash, state_keys::used_check_key};
use minter_state::{BlockStore, Checkpoint, IavlHead, NativeBlockWrite};
use num_bigint::BigInt;
use serde::{Deserialize, Serialize};
use state::{
    ABSENCE_WINDOW, Account, Amount, AppValues, PublicKey, Validator, display_key, load_validators,
    write_validators,
};
use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    path::Path,
};
use tendermint_proto::v0_34::{
    abci::{self, request::Value as Request, response::Value as Response},
    crypto::{self, public_key},
};
use transaction::{
    BancorChange, CommissionQuote, Evaluated, GasPayment, LOCKED_LIQUIDITY, LiquidityChange,
    Operation, OrderAction, Outcome, PoolCreation, Prepared, StakeChange, Transaction,
    TransactionState, commission_quote, display_address, unpayable_commission,
};

const APP_PROTOCOL_VERSION: u64 = 8;
const MAX_GAS: u64 = 100_000;
const MIN_GAS: u64 = 5_000;
const MAX_BLOCK_BYTES: i64 = 10_000_000;
const REWARD_PERIOD: u64 = 720;
const MAINNET_ORDER_LIFETIME: u64 = 483_840;
const TESTNET_ORDER_LIFETIME: u64 = 86_400;
const TOTAL_EMISSION: u128 = 10_000_000_000_000_000_000_000_000_000;

#[derive(Clone, Serialize, Deserialize)]
struct Chain {
    id: String,
    transaction_chain_id: u8,
    initial_height: u64,
    genesis_hash: [u8; 32],
    initial_validators: Vec<Validator>,
    emission: Amount,
    #[serde(default)]
    reward_price: Option<reward_price::Price>,
    #[serde(
        default,
        rename = "reward_price_time",
        skip_serializing_if = "Option::is_none"
    )]
    legacy_reward_price_time: Option<u64>,
    block_times: Vec<i64>,
    #[serde(default)]
    versions: Vec<Upgrade>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retained_candidate_rows: Vec<stakes::RetainedCandidateRow>,
    #[serde(default, skip_serializing_if = "frozen::References::is_empty")]
    frozen_references: frozen::References,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_sync: Option<native_sync::Cursor>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Upgrade {
    name: String,
    height: u64,
}

pub struct Application {
    store: BlockStore,
    network: u8,
    chain: Option<Chain>,
    fees: Option<Fees>,
    pending: Option<Block>,
    mempool: BTreeSet<Address>,
    state_sync: Option<state_sync::Receiver>,
}

struct Block {
    height: u64,
    prior_height: u64,
    chain: Chain,
    app: AppValues,
    accounts: BTreeMap<Address, Account>,
    coins: coins::Changes,
    pools: pool::Changes,
    validators: Vec<Validator>,
    removed_validators: Vec<PublicKey>,
    dropped_validators: BTreeSet<PublicKey>,
    public_keys_changed: bool,
    used_checks: BTreeSet<SigningHash>,
    present: BTreeSet<Address>,
    fees: Amount,
    candidates: Option<stakes::Candidates>,
    frozen: frozen::Changes,
    halts: halts::Votes,
    waitlist: stakes::Waitlist,
    commissions: updates::CommissionVotes,
    next_fees: Option<Fees>,
    updates: updates::NetworkVotes,
    events: Vec<Any>,
    linked_unbond_events: BTreeMap<usize, (PublicKey, UnbondEvent)>,
    ended: bool,
}

impl Application {
    pub fn open(path: &Path, transaction_chain_id: u8) -> Result<Self> {
        Self::from_store(BlockStore::open(path)?, transaction_chain_id, None)
    }

    /// Enable State Sync for a checkpoint authenticated by the operator.
    pub fn open_with_state_sync(
        path: &Path,
        transaction_chain_id: u8,
        checkpoint: Checkpoint,
    ) -> Result<Self> {
        checkpoint::validate_network(&checkpoint, transaction_chain_id)?;
        let store = BlockStore::open_for_checkpoint(path, &checkpoint)?;
        let receiver = state_sync::Receiver::new(path, checkpoint)?;
        Self::from_store(store, transaction_chain_id, Some(receiver))
    }

    fn from_store(
        store: BlockStore,
        transaction_chain_id: u8,
        state_sync: Option<state_sync::Receiver>,
    ) -> Result<Self> {
        let chain = match (store.metadata()?, store.checkpoint()) {
            (Some(bytes), checkpoint) => {
                let mut chain = serde_json::from_slice::<Chain>(&bytes)?;
                if let Some(checkpoint) = checkpoint {
                    if chain.versions.is_empty() {
                        chain.versions = checkpoint::versions(&store, checkpoint.height)?;
                    }
                    if chain.reward_price.is_none()
                        && let Some(time) = chain.legacy_reward_price_time
                    {
                        let price =
                            checkpoint::reward_price(&store)?.context("missing Go reward price")?;
                        ensure!(
                            price.time == time,
                            "legacy reward price time differs from Go snapshot"
                        );
                        chain.reward_price = Some(price);
                        chain.legacy_reward_price_time = None;
                    }
                }
                Some(chain)
            }
            (None, Some(manifest)) => Some(checkpoint::restore(
                manifest,
                transaction_chain_id,
                |name| store.checkpoint_metadata(name),
            )?),
            (None, None) => None,
        };
        let fees = if let Some(chain) = &chain {
            ensure!(
                chain.transaction_chain_id == transaction_chain_id,
                "transaction chain ID differs from genesis"
            );
            ensure!(
                chain.initial_height > 0 && chain.block_times.len() <= 4,
                "invalid execution metadata"
            );
            let head = store.head().context("missing block execution head")?;
            chain.frozen_references.validate(head.height, |height| {
                store.value_at(head.height, &frozen::key(height))
            })?;
            ensure!(
                head.height >= chain.initial_height - 1,
                "invalid execution height"
            );
            if let Some(manifest) = store.checkpoint() {
                ensure!(
                    chain.id == manifest.network,
                    "execution network differs from checkpoint"
                );
            }
            let raw = store
                .value_at(head.height, b"p")?
                .context("missing commission prices")?;
            Some(Fees::decode(&raw)?)
        } else {
            None
        };
        Ok(Self {
            store,
            network: transaction_chain_id,
            chain,
            fees,
            pending: None,
            mempool: BTreeSet::new(),
            state_sync,
        })
    }

    pub fn head(&self) -> Option<IavlHead> {
        self.store.head()
    }

    pub fn set_keep_iavl_versions(&mut self, keep: u64) -> Result<()> {
        self.store.set_keep_iavl_versions(keep)
    }

    pub fn set_keep_event_blocks(&mut self, keep: u64) -> Result<()> {
        self.store.set_keep_event_blocks(keep)
    }

    pub fn handle(&mut self, request: abci::Request) -> Result<abci::Response> {
        let request = request.value.context("empty ABCI request")?;
        let modifies_block = matches!(
            request,
            Request::BeginBlock(_)
                | Request::DeliverTx(_)
                | Request::EndBlock(_)
                | Request::Commit(_)
        );
        let response = self.dispatch(request);
        if response.is_err() && modifies_block {
            self.pending = None;
        }
        Ok(abci::Response {
            value: Some(response?),
        })
    }

    fn dispatch(&mut self, request: Request) -> Result<Response> {
        Ok(match request {
            Request::Info(_) => Response::Info(self.info()?),
            Request::InitChain(request) => Response::InitChain(self.init(request)?),
            Request::BeginBlock(request) => {
                ensure!(
                    self.chain
                        .as_ref()
                        .is_none_or(|chain| chain.native_sync.is_none()),
                    "this database follows the native Tendermint engine"
                );
                self.begin(request)?;
                Response::BeginBlock(abci::ResponseBeginBlock::default())
            }
            Request::CheckTx(request) => Response::CheckTx(
                self.check(&request.tx, transaction::DEFAULT_MIN_GAS_PRICE)?
                    .check(),
            ),
            Request::DeliverTx(request) => {
                Response::DeliverTx(self.deliver(&request.tx)?.deliver())
            }
            Request::EndBlock(request) => Response::EndBlock(self.end(request.height)?),
            Request::Commit(_) => Response::Commit(self.commit(None)?),
            Request::Query(request) => Response::Query(self.query(request)?),
            Request::Echo(request) => Response::Echo(abci::ResponseEcho {
                message: request.message,
            }),
            Request::Flush(_) => Response::Flush(abci::ResponseFlush {}),
            Request::SetOption(_) => Response::SetOption(abci::ResponseSetOption::default()),
            Request::ListSnapshots(_) => {
                Response::ListSnapshots(abci::ResponseListSnapshots::default())
            }
            Request::OfferSnapshot(request) => {
                Response::OfferSnapshot(self.offer_snapshot(request))
            }
            Request::LoadSnapshotChunk(_) => {
                Response::LoadSnapshotChunk(abci::ResponseLoadSnapshotChunk::default())
            }
            Request::ApplySnapshotChunk(request) => {
                Response::ApplySnapshotChunk(self.apply_snapshot_chunk(request))
            }
        })
    }

    fn offer_snapshot(
        &mut self,
        request: abci::RequestOfferSnapshot,
    ) -> abci::ResponseOfferSnapshot {
        use abci::response_offer_snapshot::Result as Offer;
        let result = match &mut self.state_sync {
            Some(receiver) if self.store.head().is_none() => receiver.offer(request),
            _ => Offer::Reject,
        };
        abci::ResponseOfferSnapshot {
            result: result.into(),
        }
    }

    fn apply_snapshot_chunk(
        &mut self,
        request: abci::RequestApplySnapshotChunk,
    ) -> abci::ResponseApplySnapshotChunk {
        use abci::response_apply_snapshot_chunk::Result as Apply;
        let Some(receiver) = self
            .state_sync
            .as_mut()
            .filter(|_| self.store.head().is_none())
        else {
            return state_sync::chunk_response(Apply::Abort);
        };
        let file = match receiver.apply(request) {
            state_sync::Chunk::Ready(file) => file,
            state_sync::Chunk::Reply(response) => return response,
        };
        let manifest = receiver.checkpoint();
        let restored = self.store.install_checkpoint(file, manifest, |snapshot| {
            let chain = checkpoint::restore(manifest, self.network, |name| {
                Ok(snapshot.metadata(name).map(<[u8]>::to_vec))
            })?;
            let raw = snapshot.value(b"p")?.context("missing commission prices")?;
            Ok((chain, Fees::decode(&raw)?))
        });
        match restored {
            Ok((chain, fees)) => {
                self.chain = Some(chain);
                self.fees = Some(fees);
                state_sync::chunk_response(Apply::Accept)
            }
            Err(error) => {
                eprintln!("State Sync import failed: {error:#}");
                state_sync::chunk_response(Apply::Abort)
            }
        }
    }

    fn info(&self) -> Result<abci::ResponseInfo> {
        let committed = self.store.head().filter(|head| {
            self.chain
                .as_ref()
                .is_some_and(|chain| head.height >= chain.initial_height)
        });
        let mut info = abci::ResponseInfo {
            version: concat!("minter-rust/", env!("CARGO_PKG_VERSION")).into(),
            app_version: APP_PROTOCOL_VERSION,
            ..Default::default()
        };
        if let Some(head) = committed {
            info.last_block_height = i64::try_from(head.height)?;
            info.last_block_app_hash = head.app_hash.to_vec().into();
        }
        Ok(info)
    }

    fn init(&mut self, request: abci::RequestInitChain) -> Result<abci::ResponseInitChain> {
        self.init_with(request, |_, _| Ok(false))
    }

    fn init_with(
        &mut self,
        request: abci::RequestInitChain,
        configure: impl FnOnce(&mut Chain, &abci::ResponseInitChain) -> Result<bool>,
    ) -> Result<abci::ResponseInitChain> {
        let initial_height = u64::try_from(request.initial_height)?;
        ensure!(
            initial_height != 0 && !request.chain_id.is_empty(),
            "invalid chain genesis"
        );
        if let Some(chain) = &self.chain {
            let head = self.store.head().context("missing genesis root")?;
            ensure!(
                chain.id == request.chain_id
                    && chain.initial_height == initial_height
                    && head.height < initial_height
                    && self.pending.is_none()
                    && genesis::matches_hash(
                        &request.app_state_bytes,
                        chain.genesis_hash,
                        self.store.maintenance_control()
                    )?,
                "InitChain does not match stored genesis"
            );
            let response = genesis_reply(&chain.initial_validators)?;
            let mut chain = chain.clone();
            if configure(&mut chain, &response)? {
                let metadata = genesis_metadata(&mut chain, head)?;
                self.store.update_genesis_metadata(head.height, &metadata)?;
                self.chain = Some(chain);
            }
            return Ok(response);
        }

        let genesis = genesis::prepare(&self.store, self.network, &request.app_state_bytes)?;
        let response = genesis_reply(&genesis.validators)?;
        let mut chain = Chain {
            id: request.chain_id,
            transaction_chain_id: self.network,
            initial_height,
            genesis_hash: genesis::hash(
                &request.app_state_bytes,
                self.store.maintenance_control(),
            )?,
            initial_validators: genesis.validators,
            emission: genesis.emission,
            reward_price: Some(genesis.reward_price),
            legacy_reward_price_time: None,
            block_times: Vec::new(),
            versions: vec![Upgrade {
                name: "v300".into(),
                height: initial_height - 1,
            }],
            retained_candidate_rows: Vec::new(),
            frozen_references: frozen::References::default(),
            native_sync: None,
        };
        configure(&mut chain, &response)?;
        self.store
            .initialize_with_metadata(initial_height - 1, genesis.operations, |head| {
                genesis_metadata(&mut chain, head)
            })?;
        self.chain = Some(chain);
        self.fees = Some(genesis.fees);
        Ok(response)
    }

    fn begin(&mut self, request: abci::RequestBeginBlock) -> Result<()> {
        ensure!(self.pending.is_none(), "previous block has not committed");
        let head = self.store.head().context("genesis is unavailable")?;
        let mut chain = self.chain.clone().context("genesis is unavailable")?;
        let header = request.header.context("missing block header")?;
        let height = u64::try_from(header.height)?;
        ensure!(
            header.chain_id == chain.id && height == head.height + 1,
            "unexpected block chain or height {height}"
        );
        if self.store.checkpoint().is_some() {
            ensure!(
                header.app_hash.as_ref() == head.app_hash,
                "previous AppHash differs at height {height}"
            );
        }
        let time = header.time.context("missing block time")?;
        ensure!(
            (0..1_000_000_000).contains(&time.nanos),
            "invalid block timestamp"
        );
        let hour = time.seconds.div_euclid(3600).rem_euclid(24);
        let timestamp = i128::from(time.seconds) * 1_000_000_000 + i128::from(time.nanos);
        let previous_time = chain
            .reward_price
            .as_ref()
            .map(|price| price.time)
            .or(chain.legacy_reward_price_time);
        let emission_open = chain.emission < Amount::from(TOTAL_EMISSION);
        let mut app = AppValues::load(&self.store, head.height)?;
        let mut events = Vec::new();
        if !emission_open {
            app.set_reward(Amount::ZERO, Amount::ZERO);
        } else if height % REWARD_PERIOD == 1
            && previous_time.is_none_or(|previous| {
                (12..=14).contains(&hour)
                    && timestamp - i128::from(previous as i64) > 10_800 * 1_000_000_000
            })
        {
            ensure!(
                chain.legacy_reward_price_time.is_none(),
                "previous reward price reserves are missing"
            );
            let pools = pool::View {
                store: &self.store,
                height: head.height,
                changes: None,
            };
            let pool = pools
                .load(0, reward_price::USDT)?
                .context("missing BIP/USDT reward price pool")?;
            let (bip, usdt) = pool.reserves(0, reward_price::USDT);
            let fixed = chain
                .versions
                .iter()
                .any(|upgrade| upgrade.name == "v320" && height > upgrade.height + 1);
            let (price, safe) = reward_price::Price::update(
                chain.reward_price.as_ref(),
                timestamp as i64 as u64,
                bip,
                usdt,
                fixed,
            )?;
            events.push(pack(UpdatedBlockRewardEvent {
                value: price.last.to_string(),
                value_locked_stake_rewards: (&safe * 3u32).to_string(),
            }));
            app.set_reward(price.last.clone(), safe);
            chain.reward_price = Some(price);
        }
        let max_gas = next_gas(app.max_gas, &chain.block_times);
        if app.max_gas != max_gas {
            app.max_gas = max_gas;
            app.dirty = true;
        }
        chain.block_times.push(time.seconds);
        if chain.block_times.len() > 4 {
            chain.block_times.remove(0);
        }
        let mut validators = load_validators(&self.store, head.height)?;
        let mut removed = Vec::new();
        if height == chain.initial_height {
            removed.extend(validators.iter().filter_map(|old| {
                (!chain
                    .initial_validators
                    .iter()
                    .any(|new| new.key == old.key))
                .then_some(old.key)
            }));
            validators.clone_from(&chain.initial_validators);
            for validator in &mut validators {
                validator.dirty = true;
            }
        }
        let mut present = BTreeSet::new();
        let mut absent_drops = Vec::new();
        if let Some(commit) = request.last_commit_info {
            for vote in commit.votes {
                let voter = vote.validator.context("missing vote validator")?;
                let address: Address = voter.address.as_ref().try_into()?;
                if let Some(validator) = validators.iter_mut().find(|v| v.address() == address) {
                    let old = validator.absent;
                    let mask = 1u32 << (height % u64::from(ABSENCE_WINDOW));
                    if vote.signed_last_block {
                        present.insert(address);
                        validator.absent &= !mask;
                    } else {
                        present.remove(&address);
                        validator.absent |= mask;
                        if validator.absent.count_ones() > 12 {
                            validator.absent = 0;
                            validator.dirty = true;
                            absent_drops.push(validator.key);
                        }
                    }
                    validator.dirty |= old != validator.absent;
                }
            }
        }
        let frozen = frozen::Changes::with_references(std::mem::take(&mut chain.frozen_references));
        let mut block = Block {
            height,
            prior_height: head.height,
            chain,
            app,
            accounts: BTreeMap::new(),
            coins: coins::Changes::default(),
            pools: pool::Changes::default(),
            validators,
            removed_validators: removed,
            dropped_validators: BTreeSet::new(),
            public_keys_changed: false,
            used_checks: BTreeSet::new(),
            present,
            fees: Amount::default(),
            candidates: None,
            frozen,
            halts: halts::Votes::begin(&self.store, head.height, height)?,
            waitlist: stakes::Waitlist::default(),
            commissions: updates::CommissionVotes::default(),
            next_fees: None,
            updates: updates::NetworkVotes::default(),
            events,
            linked_unbond_events: BTreeMap::new(),
            ended: false,
        };
        if !absent_drops.is_empty() {
            let start = block.chain.initial_height - 1;
            let in_grace = (start..=start + 120).contains(&height)
                || block
                    .chain
                    .versions
                    .iter()
                    .any(|upgrade| (upgrade.height..=upgrade.height + 120).contains(&height));
            let jail_period = if self.network == 2 {
                2 * frozen::TESTNET_QUARTER_HOUR
            } else {
                17_280
            };
            let jail = (!in_grace).then_some(height + jail_period);
            for key in absent_drops {
                block.set_candidate_status(&self.store, key, false, jail)?;
            }
        }
        block.enforce_halt()?;
        let version = block
            .chain
            .versions
            .iter()
            .rev()
            .find(|version| version.height <= height)
            .map_or("v300", |version| version.name.as_str());
        ensure!(
            matches!(version, "v300" | "v310" | "v320" | "v330"),
            "network upgrade {version} requires a compatible node at height {height}"
        );
        block.punish_evidence(&self.store, request.byzantine_validators)?;
        block.mature_funds(&self.store)?;
        self.pending = Some(block);
        Ok(())
    }

    fn check(&mut self, raw: &[u8], min_gas_price: u32) -> Result<Outcome> {
        let head = self.store.head().context("genesis is unavailable")?;
        let fees = self.fees.as_ref().context("missing commission prices")?;
        let view = coins::View {
            store: &self.store,
            height: head.height,
            network: self.network,
            changes: None,
        };
        let tx = match Transaction::prepare(raw, fees, &view, Some(min_gas_price))? {
            Prepared::Transaction(tx) => tx,
            Prepared::Rejected(outcome) => return Ok(outcome),
        };
        let mut account = Account::load(&self.store, head.height, tx.sender)?;
        if let Some(rejected) = tx.early_rejection(&account, fees.price_coin) {
            return Ok(rejected);
        }
        let price = match tx.quote(&view, None, fees.price_coin)? {
            CommissionQuote::Price(price) => price,
            CommissionQuote::Rejected(outcome) => return Ok(outcome),
        };
        let state = TransactionState {
            stakes: stakes::View {
                store: &self.store,
                height: head.height,
                candidates: None,
                waitlist: None,
                validator_count: None,
                retained: &self
                    .chain
                    .as_ref()
                    .context("genesis is unavailable")?
                    .retained_candidate_rows,
            },
            halts: None,
            commissions: None,
            updates: None,
            used_checks: None,
        };
        let mut evaluated = tx.evaluate(&view, None, &state, &mut account, price, true)?;
        if let Some(address) = evaluated.account_to_check() {
            let mut target = Account::load(&self.store, head.height, address)?;
            evaluated = tx.check_related_account(evaluated, &view, &mut target)?;
        }
        if let Evaluated::Rejected(outcome) = evaluated {
            return Ok(outcome);
        }
        if !self.mempool.insert(tx.sender) {
            let sender = display_address(tx.sender);
            return Ok(Outcome::reject(
                113,
                format!("Tx from {sender} already exists in mempool"),
                [
                    ("sender", sender),
                    ("block_height", (head.height + 1).to_string()),
                ],
            ));
        }
        Ok(tx.success())
    }

    fn deliver(&mut self, raw: &[u8]) -> Result<Outcome> {
        let block = self.pending.as_mut().context("no active block")?;
        ensure!(!block.ended, "block already ended");
        let fees = self.fees.as_ref().context("missing commission prices")?;
        let height = block.prior_height;
        let view = coins::View {
            store: &self.store,
            height,
            network: self.network,
            changes: Some(&block.coins),
        };
        let tx = match Transaction::prepare(raw, fees, &view, None)? {
            Prepared::Transaction(tx) => tx,
            Prepared::Rejected(outcome) => return Ok(outcome),
        };
        let sender = account_at(&mut block.accounts, &self.store, height, tx.sender)?;
        if let Some(rejected) = tx.early_rejection(sender, fees.price_coin) {
            return Ok(rejected);
        }
        let price = match tx.quote(&view, Some(&block.pools), fees.price_coin)? {
            CommissionQuote::Price(price) => price,
            CommissionQuote::Rejected(outcome) => return Ok(outcome),
        };
        let state = TransactionState {
            stakes: stakes::View {
                store: &self.store,
                height,
                candidates: block.candidates.as_ref(),
                waitlist: Some(&block.waitlist),
                validator_count: Some(block.validators.len()),
                retained: &block.chain.retained_candidate_rows,
            },
            halts: Some(&block.halts),
            commissions: Some(&block.commissions),
            updates: Some(&block.updates),
            used_checks: Some(&block.used_checks),
        };
        let mut evaluated = tx.evaluate(&view, Some(&block.pools), &state, sender, price, false)?;
        if let Some(address) = evaluated.account_to_check() {
            let target = account_at(&mut block.accounts, &self.store, height, address)?;
            evaluated = tx.check_related_account(evaluated, &view, target)?;
        }
        let (gas, operation) = match evaluated {
            Evaluated::Accepted { gas, operation } => (gas, operation),
            Evaluated::Rejected(mut outcome) => {
                let failed_quote = match commission_quote(
                    &view,
                    Some(&block.pools),
                    fees.price_coin,
                    &tx.failed_fee,
                )? {
                    CommissionQuote::Price(price) => price,
                    CommissionQuote::Rejected(outcome) => return Ok(outcome),
                };
                let pools = pool::View {
                    store: &self.store,
                    height,
                    changes: Some(&block.pools),
                };
                let gas = match GasPayment::choose(&view, &pools, tx.gas_coin, failed_quote)? {
                    Ok(gas) => gas,
                    Err(outcome) => return Ok(outcome),
                };
                let payer = match tx.failed_payer(&mut outcome) {
                    Ok(payer) => payer,
                    Err(rejected) => return Ok(rejected),
                };
                let balance = account_at(&mut block.accounts, &self.store, height, payer)?
                    .balance(&self.store, height, tx.gas_coin)?;
                let gas = if balance.bits() == 0 {
                    None
                } else {
                    let payment = match gas.limit_failed(&view, &pools, fees.price_coin, balance)? {
                        Ok(payment) => payment,
                        Err(outcome) => return Ok(outcome),
                    };
                    block.pay_gas(&self.store, payer, &payment)?;
                    Some(payment)
                };
                tx.delivered(&mut outcome, fees.price_coin, gas.as_ref(), None, None)?;
                return Ok(outcome);
            }
        };
        let payer = match &operation {
            Operation::RedeemCheck(check) => check.issuer,
            _ => tx.sender,
        };
        block.pay_gas(&self.store, payer, &gas)?;
        let mut outcome = tx.success();
        if let Some(creation) = tx.creation() {
            let id = block.app.coins.checked_add(1).context("coin ID overflow")?;
            let sender = block.account(&self.store, tx.sender)?;
            if creation.supply.reserve.bits() != 0 {
                sender.sub_balance(&self.store, height, 0, &creation.supply.reserve)?;
            }
            sender.add_balance(&self.store, height, id, &creation.supply.volume)?;
            sender.set_nonce(tx.nonce);
            block.coins.create(
                id,
                creation.coin.clone(),
                creation.supply.clone(),
                tx.sender,
            );
            block.app.coins = id;
            block.app.dirty = true;

            // Go quotes the ticker separately after the transaction's state changes.
            let ticker_price = tx.ticker_price.as_ref().context("missing ticker price")?;
            let view = block.coins_view(&self.store);
            let ticker_burn =
                match commission_quote(&view, Some(&block.pools), fees.price_coin, ticker_price)? {
                    CommissionQuote::Price(price) => price,
                    CommissionQuote::Rejected(outcome) => return Ok(outcome),
                };
            if ticker_burn.bits() == 0 {
                return Ok(unpayable_commission());
            }
            ensure!(
                block.fees >= ticker_burn,
                "ticker charge exceeds the reward pool"
            );
            block.fees -= &ticker_burn;
            block
                .account(&self.store, Address::default())?
                .add_balance(&self.store, height, 0, &ticker_burn)?;
            tx.delivered(
                &mut outcome,
                fees.price_coin,
                Some(&gas),
                Some((id, &ticker_burn)),
                None,
            )?;
            return Ok(outcome);
        }
        if let Some(change) = tx.supply_change() {
            let mut supply = block
                .coins_view(&self.store)
                .supply(change.id)?
                .into_owned();
            let sender = block.account(&self.store, tx.sender)?;
            if change.mint {
                supply.volume += &change.value;
                sender.add_balance(&self.store, height, change.id, &change.value)?;
            } else {
                ensure!(
                    supply.volume > change.value,
                    "burn amount exceeds token supply"
                );
                supply.volume -= &change.value;
                sender.sub_balance(&self.store, height, change.id, &change.value)?;
            }
            block.coins.set_supply(change.id, supply);
        }
        match &operation {
            Operation::Recreate(change) => {
                let creation = tx.recreation().context("missing coin recreation data")?;
                ensure!(
                    change.id == block.app.coins.checked_add(1).context("coin ID overflow")?,
                    "recreation coin ID changed after planning"
                );
                let sender = block.account(&self.store, tx.sender)?;
                if creation.supply.reserve.bits() != 0 {
                    sender.sub_balance(&self.store, height, 0, &creation.supply.reserve)?;
                }
                sender.add_balance(&self.store, height, change.id, &creation.supply.volume)?;
                block.coins.recreate(
                    change.previous_id,
                    change.previous.clone(),
                    change.history.clone(),
                    change.id,
                    creation.coin.clone(),
                    creation.supply.clone(),
                );
                block.app.coins = change.id;
                block.app.dirty = true;
            }
            Operation::Swap(swap) => {
                let last = swap.steps.len() - 1;
                let (debit, credit) = if swap.buying { (last, 0) } else { (0, last) };
                for (index, step) in swap.steps.iter().enumerate() {
                    block.apply_swap_step(&self.store, step)?;
                    if index == debit {
                        block.account(&self.store, tx.sender)?.sub_balance(
                            &self.store,
                            height,
                            step.coin_in,
                            &step.input,
                        )?;
                    }
                    if index == credit {
                        block.account(&self.store, tx.sender)?.add_balance(
                            &self.store,
                            height,
                            step.coin_out,
                            &step.output,
                        )?;
                    }
                }
            }
            Operation::LimitOrder(order) => block.apply_order(&self.store, order)?,
            Operation::Liquidity(change) => {
                block.apply_liquidity(&self.store, tx.sender, change)?
            }
            Operation::CreatePool(change) => block.create_pool(&self.store, tx.sender, change)?,
            Operation::Bancor(change) => block.apply_bancor(&self.store, tx.sender, change)?,
            Operation::Stake(change) => block.apply_stake(&self.store, tx.sender, change)?,
            Operation::Multisig(change) => block
                .account(&self.store, change.address)?
                .set_multisig(change.model.clone()),
            Operation::RedeemCheck(check) => {
                block.used_checks.insert(check.hash);
                block.account(&self.store, check.issuer)?.sub_balance(
                    &self.store,
                    height,
                    check.coin,
                    &check.amount,
                )?;
                block.account(&self.store, tx.sender)?.add_balance(
                    &self.store,
                    height,
                    check.coin,
                    &check.amount,
                )?;
            }
            Operation::EditCoinOwner(change) => {
                block
                    .coins
                    .set_owner(change.id, change.edit.symbol, change.edit.owner)
            }
            Operation::SetHaltBlock(vote) => block.halts.record(&self.store, height, *vote)?,
            Operation::VoteCommission(vote) => {
                block.commissions.record(&self.store, height, vote)?
            }
            Operation::VoteUpdate(vote) => block.updates.record(&self.store, height, vote)?,
            Operation::Regular => {}
        }
        for payment in tx.payments() {
            block.account(&self.store, tx.sender)?.sub_balance(
                &self.store,
                height,
                payment.coin,
                &payment.value,
            )?;
            block.account(&self.store, payment.recipient)?.add_balance(
                &self.store,
                height,
                payment.coin,
                &payment.value,
            )?;
        }
        block.account(&self.store, tx.sender)?.set_nonce(tx.nonce);
        tx.delivered(
            &mut outcome,
            fees.price_coin,
            Some(&gas),
            None,
            Some(&operation),
        )?;
        Ok(outcome)
    }

    fn end(&mut self, height: i64) -> Result<abci::ResponseEndBlock> {
        let block = self.pending.as_mut().context("no active block")?;
        ensure!(
            !block.ended && u64::try_from(height)? == block.height,
            "unexpected EndBlock height {height}"
        );
        let emission_open = block.chain.emission < Amount::from(TOTAL_EMISSION);
        let mut reward_pool = block.fees.clone();
        if emission_open {
            reward_pool += &block.app.reward;
        }
        for validator in &mut block.validators {
            if block.dropped_validators.contains(&validator.key) {
                reward_pool += std::mem::take(&mut validator.reward);
                validator.reward_dirty = true;
            }
        }

        let total_stake: Amount = block
            .validators
            .iter()
            .filter(|v| {
                !block.dropped_validators.contains(&v.key) && block.present.contains(&v.address())
            })
            .map(|v| &v.stake)
            .sum();
        let denominator = total_stake.max(Amount::from(1u32));
        let mut remainder = reward_pool.clone();
        for validator in &mut block.validators {
            if !block.dropped_validators.contains(&validator.key)
                && block.present.contains(&validator.address())
            {
                let share = &reward_pool * &validator.stake / &denominator;
                remainder -= &share;
                if share.bits() != 0 {
                    validator.reward += share;
                    validator.reward_dirty = true;
                }
            }
        }
        if remainder.bits() != 0 {
            block.app.slashed += remainder;
            block.app.dirty = true;
        }
        let order_lifetime = if self.network == 2 {
            TESTNET_ORDER_LIFETIME
        } else {
            MAINNET_ORDER_LIFETIME
        };
        if block.height > order_lifetime && block.height % REWARD_PERIOD == REWARD_PERIOD / 2 {
            let prior_height = block.prior_height;
            let refunds = block.pools.orders.expire(
                &self.store,
                prior_height,
                block.height - order_lifetime,
            )?;
            for refund in refunds {
                block.refund_expired_order(&self.store, &refund)?;
            }
        }
        let payout = block.height.is_multiple_of(REWARD_PERIOD);
        if payout {
            block.pay_rewards(&self.store, emission_open)?;
        }
        if emission_open {
            block.chain.emission += &block.app.safe_reward;
            if block.app.safe_reward > block.app.reward {
                let difference = &block.app.safe_reward - &block.app.reward;
                let prior_height = block.prior_height;
                block
                    .account(&self.store, Address::default())?
                    .add_balance(&self.store, prior_height, 0, &difference)?;
            }
        }
        let vote_power = |key: &PublicKey| {
            block
                .validators
                .iter()
                .find(|validator| {
                    &validator.key == key
                        && block.present.contains(&validator.address())
                        && !block.dropped_validators.contains(key)
                })
                .map(|validator| &validator.stake)
        };
        if let Some(price) = block.commissions.resolve(
            &self.store,
            block.prior_height,
            block.height,
            vote_power,
            &denominator,
        )? {
            let fees = Fees::decode(&price)?;
            block.events.push(pack(fees.event()));
            block.next_fees = Some(fees);
        }
        if let Some(version) = block.updates.resolve(
            &self.store,
            block.prior_height,
            block.height,
            vote_power,
            &denominator,
        )? {
            let version = String::from_utf8(version).context("invalid network version name")?;
            block.chain.versions.push(Upgrade {
                name: version.clone(),
                height: block.height,
            });
            block.events.push(pack(UpdateNetworkEvent { version }));
        }
        let validator_updates =
            if payout || !block.dropped_validators.is_empty() || block.public_keys_changed {
                block.update_validators(&self.store, self.network)?
            } else {
                Vec::new()
            };
        block.ended = true;
        Ok(abci::ResponseEndBlock {
            validator_updates,
            consensus_param_updates: Some(abci::ConsensusParams {
                block: Some(abci::BlockParams {
                    max_bytes: MAX_BLOCK_BYTES,
                    max_gas: i64::try_from(block.app.max_gas)?,
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn commit(&mut self, native: Option<NativeBlockWrite<'_>>) -> Result<abci::ResponseCommit> {
        let mut block = self.pending.take().context("no active block")?;
        ensure!(block.ended, "EndBlock is required before Commit");
        let mut operations = Vec::new();
        for account in block.accounts.into_values().rev() {
            account.write(&mut operations);
        }
        block.app.write(&mut operations);
        block.coins.write(&mut operations);
        if let Some(candidates) = block.candidates {
            candidates.write(
                &self.store,
                block.prior_height,
                &mut operations,
                &mut block.chain.retained_candidate_rows,
            )?;
        }
        write_validators(&block.validators, block.removed_validators, &mut operations);
        for hash in block.used_checks.into_iter().rev() {
            operations.push(state::set(used_check_key(hash), vec![1]));
        }
        block.chain.frozen_references = block.frozen.write(&mut operations)?;
        block.halts.write(&mut operations);
        block.waitlist.write(&mut operations);
        block.pools.write(&mut operations)?;
        if let Some(fees) = &block.next_fees {
            operations.push(state::set(b"p".to_vec(), fees.encode()));
        }
        block.commissions.write(&mut operations);
        block.updates.write(&mut operations);
        let events = encode_events(block.events);
        let head = match native {
            Some(native) => self.store.commit_native_block(
                block.height,
                operations,
                |head| {
                    block
                        .chain
                        .native_sync
                        .as_mut()
                        .context("missing staged native cursor")?
                        .set_committed_root(head.app_hash);
                    Ok(serde_json::to_vec(&block.chain)?)
                },
                &events,
                native,
            )?,
            None => self.store.commit_with_events(
                block.height,
                operations,
                &serde_json::to_vec(&block.chain)?,
                &events,
            )?,
        };
        self.chain = Some(block.chain);
        if let Some(fees) = block.next_fees {
            self.fees = Some(fees);
        }
        self.mempool.clear();
        Ok(abci::ResponseCommit {
            data: head.app_hash.to_vec().into(),
            retain_height: 0,
        })
    }

    fn query(&self, request: abci::RequestQuery) -> Result<abci::ResponseQuery> {
        let iavl = request.path == minter_iavl::QUERY_PATH;
        if !iavl && request.path != "/balance" {
            return Ok(abci::ResponseQuery::default());
        }
        let head = self.store.head().context("genesis is unavailable")?;
        if iavl && (!request.prove || request.height < 0) {
            return Ok(abci::ResponseQuery {
                code: 1,
                log: "IAVL key query requires prove=true and an available height".into(),
                ..Default::default()
            });
        }
        if !iavl && (request.prove || !matches!(request.data.len(), 20 | 24) || request.height < 0)
        {
            return Ok(abci::ResponseQuery {
                code: 1,
                log: "balance requires a 20-byte address, optional 4-byte coin ID and available height".into(),
                ..Default::default()
            });
        }
        let height = if request.height == 0 {
            head.height
        } else {
            request.height as u64
        };
        let earliest = self
            .store
            .earliest_height()
            .context("missing historical height range")?;
        if !(earliest..=head.height).contains(&height) {
            let kind = if iavl { "IAVL proof" } else { "balance" };
            return Ok(abci::ResponseQuery {
                code: 1,
                log: format!(
                    "{kind} is available at heights {earliest}..={}",
                    head.height
                ),
                ..Default::default()
            });
        }
        if iavl {
            let result = self.store.proof_at(height, &request.data)?;
            return Ok(abci::ResponseQuery {
                height: i64::try_from(height)?,
                key: request.data.clone(),
                value: result.value.unwrap_or_default().into(),
                proof_ops: Some(crypto::ProofOps {
                    ops: vec![crypto::ProofOp {
                        r#type: minter_iavl::PROOF_OP_TYPE.into(),
                        key: request.data.to_vec(),
                        data: result.proof,
                    }],
                }),
                ..Default::default()
            });
        }
        let address = request.data[..20].try_into()?;
        let coin = request.data.get(20..24).map_or(0, |bytes| {
            u32::from_le_bytes(bytes.try_into().expect("four-byte coin ID"))
        });
        let mut account = Account::load(&self.store, height, address)?;
        let balance = account.balance(&self.store, height, coin)?.to_string();
        Ok(abci::ResponseQuery {
            height: i64::try_from(height)?,
            key: request.data,
            value: serde_json::to_vec(&serde_json::json!({
                "balance": balance, "nonce": account.nonce,
            }))?
            .into(),
            ..Default::default()
        })
    }
}

impl Block {
    fn enforce_halt(&self) -> Result<()> {
        let voters = self.halts.scheduled();
        if voters.is_empty() {
            return Ok(());
        }
        let eligible = self.validators.iter().filter(|validator| {
            self.present.contains(&validator.address())
                && !self.dropped_validators.contains(&validator.key)
        });
        let total: Amount = eligible.clone().map(|validator| &validator.stake).sum();
        let voted: Amount = voters
            .iter()
            .filter_map(|key| eligible.clone().find(|validator| &validator.key == key))
            .map(|validator| &validator.stake)
            .sum();
        ensure!(
            !updates::has_quorum(&voted, &total),
            "application halted by validator votes at height {}",
            self.height
        );
        Ok(())
    }

    fn account(&mut self, store: &BlockStore, address: Address) -> Result<&mut Account> {
        account_at(&mut self.accounts, store, self.prior_height, address)
    }

    fn apply_bancor(
        &mut self,
        store: &BlockStore,
        sender: Address,
        change: &BancorChange,
    ) -> Result<()> {
        let height = self.prior_height;
        let account = self.account(store, sender)?;
        account.sub_balance(store, height, change.from, &change.sold)?;
        account.add_balance(store, height, change.to, &change.bought)?;
        if change.from != 0 {
            let mut supply = self.coins_view(store).supply(change.from)?.into_owned();
            ensure!(
                supply.volume >= change.sold && supply.reserve >= change.reserve,
                "sale exceeds coin supply or reserve"
            );
            supply.volume -= &change.sold;
            supply.reserve -= &change.reserve;
            self.coins.set_supply(change.from, supply);
        }
        if change.to != 0 {
            let mut supply = self.coins_view(store).supply(change.to)?.into_owned();
            supply.volume += &change.bought;
            supply.reserve += &change.reserve;
            self.coins.set_supply(change.to, supply);
        }
        Ok(())
    }

    fn create_pool(
        &mut self,
        store: &BlockStore,
        sender: Address,
        change: &PoolCreation,
    ) -> Result<()> {
        ensure!(
            self.app.coins.checked_add(1) == Some(change.coin_id),
            "unexpected pool token ID"
        );
        let height = self.prior_height;
        self.pools
            .create_pool(change.pair, change.volumes.clone(), change.pool_id)?;
        let account = self.account(store, sender)?;
        for (coin, amount) in change.pair.into_iter().zip(&change.volumes) {
            account.sub_balance(store, height, coin, amount)?;
        }
        account.add_balance(store, height, change.coin_id, &change.minted)?;
        self.account(store, Address::default())?.add_balance(
            store,
            height,
            change.coin_id,
            &Amount::from(LOCKED_LIQUIDITY),
        )?;
        let (coin, supply) = change.token();
        self.coins.register(change.coin_id, coin, supply, None);
        self.app.coins = change.coin_id;
        self.app.dirty = true;
        Ok(())
    }

    fn apply_liquidity(
        &mut self,
        store: &BlockStore,
        sender: Address,
        change: &LiquidityChange,
    ) -> Result<()> {
        let height = self.prior_height;
        let mut supply = self.coins_view(store).supply(change.share_id)?.into_owned();
        let account = self.account(store, sender)?;
        for (coin, amount) in change.pair.into_iter().zip(&change.volumes) {
            if change.adding {
                account.sub_balance(store, height, coin, amount)?;
            } else {
                account.add_balance(store, height, coin, amount)?;
            }
        }
        if change.adding {
            supply.volume += &change.shares;
            account.add_balance(store, height, change.share_id, &change.shares)?;
        } else {
            ensure!(
                supply.volume >= change.shares,
                "burn exceeds pool token supply"
            );
            supply.volume -= &change.shares;
            account.sub_balance(store, height, change.share_id, &change.shares)?;
        }
        self.coins.set_supply(change.share_id, supply);
        self.pools.replace(change.pool.clone());
        Ok(())
    }

    fn coins_view<'a>(&'a self, store: &'a BlockStore) -> coins::View<'a> {
        coins::View {
            store,
            height: self.prior_height,
            network: self.chain.transaction_chain_id,
            changes: Some(&self.coins),
        }
    }

    fn apply_swap_step(&mut self, store: &BlockStore, step: &pool::SwapStep) -> Result<()> {
        self.pools.apply(step);
        let height = self.prior_height;
        for refund in &step.refunds {
            self.refund_expired_order(store, refund)?;
        }
        self.account(store, pool::BURN_ADDRESS)?.add_balance(
            store,
            height,
            step.coin_in,
            &step.burned,
        )?;
        for (&owner, amount) in &step.makers {
            self.account(store, owner)?
                .add_balance(store, height, step.coin_in, amount)?;
        }
        Ok(())
    }

    fn refund_expired_order(&mut self, store: &BlockStore, refund: &orders::Refund) -> Result<()> {
        let height = self.prior_height;
        self.account(store, refund.owner)?.add_balance(
            store,
            height,
            refund.coin,
            &refund.value,
        )?;
        self.events.push(pack(refund.event()));
        Ok(())
    }

    fn pay_gas(&mut self, store: &BlockStore, sender: Address, gas: &GasPayment) -> Result<()> {
        match gas {
            GasPayment::Base(_) => {}
            GasPayment::Pool(step) => self.apply_swap_step(store, step)?,
            GasPayment::Reserve {
                coin,
                amount,
                reward,
            } => {
                let mut supply = self.coins_view(store).supply(*coin)?.into_owned();
                ensure!(
                    &supply.volume >= amount && &supply.reserve >= reward,
                    "gas charge exceeds coin supply or reserve"
                );
                supply.volume -= amount;
                supply.reserve -= reward;
                self.coins.set_supply(*coin, supply);
            }
        }
        let height = self.prior_height;
        self.account(store, sender)?
            .sub_balance(store, height, gas.coin(), gas.amount())?;
        self.fees += gas.reward();
        Ok(())
    }

    fn apply_order(&mut self, store: &BlockStore, action: &OrderAction) -> Result<()> {
        let height = self.prior_height;
        match action {
            OrderAction::Place { order, .. } => {
                let escrow = order.refund();
                self.account(store, escrow.owner)?.sub_balance(
                    store,
                    height,
                    escrow.coin,
                    &escrow.value,
                )?;
                self.pools.orders.place(store, height, order.clone())?;
            }
            OrderAction::Cancel { order_id, .. } => {
                let refund = self.pools.orders.cancel(store, height, *order_id)?;
                self.account(store, refund.owner)?.add_balance(
                    store,
                    height,
                    refund.coin,
                    &refund.value,
                )?;
            }
        }
        Ok(())
    }

    fn mature_funds(&mut self, store: &BlockStore) -> Result<()> {
        let prior = self.prior_height;
        let Some(funds) = self.frozen.release(store, prior, self.height)? else {
            return Ok(());
        };
        for fund in funds {
            let destination = fund.destination();
            let address = display_address(fund.owner);
            let amount = fund.value.to_string();
            let coin = u64::from(fund.coin);
            if destination == 0 {
                self.account(store, fund.owner)?.add_balance(
                    store,
                    prior,
                    fund.coin,
                    &fund.value,
                )?;
                let event = if let Some(key) = fund.source {
                    let event = UnbondEvent {
                        address,
                        amount,
                        coin,
                        validator_pub_key: display_key(key),
                    };
                    if fund.has_linked_source() {
                        self.linked_unbond_events
                            .insert(self.events.len(), (key, event.clone()));
                    }
                    pack(event)
                } else {
                    pack(UnlockEvent {
                        address,
                        amount,
                        coin,
                    })
                };
                self.events.push(event);
            } else {
                let source = fund
                    .source
                    .context("frozen stake move lacks a source candidate")?;
                let key = self.candidates_mut(store)?.delegate(
                    destination,
                    fund.owner,
                    fund.coin,
                    fund.value,
                )?;
                self.events.push(pack(StakeMoveEvent {
                    address,
                    amount,
                    coin,
                    candidate_pub_key: display_key(source),
                    to_candidate_pub_key: display_key(key),
                }));
            }
        }
        Ok(())
    }

    fn candidates_mut(&mut self, store: &BlockStore) -> Result<&mut stakes::Candidates> {
        if self.candidates.is_none() {
            self.candidates = Some(stakes::Candidates::load(
                store,
                self.prior_height,
                &self.chain.retained_candidate_rows,
            )?);
        }
        self.candidates.as_mut().context("missing candidate state")
    }

    fn set_candidate_status(
        &mut self,
        store: &BlockStore,
        key: PublicKey,
        online: bool,
        jail: Option<u64>,
    ) -> Result<()> {
        self.candidates_mut(store)?
            .get_mut(key)?
            .set_status(online, jail);
        if let Some(jailed_until) = jail {
            self.events.push(pack(JailEvent {
                validator_pub_key: display_key(key),
                jailed_until,
            }));
        }
        if !online && self.validators.iter().any(|validator| validator.key == key) {
            self.dropped_validators.insert(key);
        }
        Ok(())
    }

    fn apply_stake(
        &mut self,
        store: &BlockStore,
        owner: Address,
        change: &StakeChange,
    ) -> Result<()> {
        let prior = self.prior_height;
        match change {
            StakeChange::Declare(data) => {
                self.account(store, owner)?
                    .sub_balance(store, prior, data.coin, &data.value)?;
                let height = self.height;
                let candidates = self.candidates_mut(store)?;
                let id =
                    candidates.declare(data.key, data.owner, owner, data.commission, height)?;
                candidates.delegate(id, owner, data.coin, data.value.clone())?;
            }
            StakeChange::Delegate {
                id,
                coin,
                value,
                waiting,
                ..
            } => {
                self.account(store, owner)?
                    .sub_balance(store, prior, *coin, value)?;
                let mut total = value.clone();
                if let Some(waiting) = waiting {
                    self.waitlist
                        .withdraw(store, prior, owner, *id, *coin, waiting)?;
                    total += waiting;
                }
                self.candidates_mut(store)?
                    .delegate(*id, owner, *coin, total)?;
            }
            StakeChange::Withdraw {
                from,
                source_id,
                destination_id,
                coin,
                value,
                waiting,
                due,
                ..
            } => {
                let active = if let Some(waiting) = waiting {
                    self.waitlist
                        .withdraw(store, prior, owner, *source_id, *coin, value)?;
                    (value > waiting).then(|| value - waiting)
                } else {
                    Some(value.clone())
                };
                if let Some(active) = active {
                    self.candidates_mut(store)?
                        .subtract_stake(*source_id, owner, *coin, &active)?;
                }
                self.frozen.push(
                    store,
                    prior,
                    *due,
                    frozen::Fund::new(
                        owner,
                        *coin,
                        value.clone(),
                        Some((*from, *source_id)),
                        *destination_id,
                    ),
                )?;
            }
            StakeChange::Lock { due, coin, value } => {
                self.account(store, owner)?
                    .sub_balance(store, prior, *coin, value)?;
                self.frozen.push(
                    store,
                    prior,
                    *due,
                    frozen::Fund::new(owner, *coin, value.clone(), None, 0),
                )?;
            }
            StakeChange::LockStake(due) => self.account(store, owner)?.set_stake_lock(*due),
            StakeChange::CandidateStatus { key, online, .. } => {
                self.set_candidate_status(store, *key, *online, None)?;
            }
            StakeChange::Edit(data) => self
                .candidates_mut(store)?
                .get_mut(data.key)?
                .set_addresses(data.owner, data.reward, data.control),
            StakeChange::EditCommission { key, commission } => {
                let height = self.height;
                self.candidates_mut(store)?
                    .get_mut(*key)?
                    .set_commission(*commission, height);
            }
            StakeChange::EditPublicKey { key, new, .. } => {
                self.candidates_mut(store)?.change_key(*key, *new)?;
                self.frozen.rotate_key(*key, *new);
                for (&index, (source, event)) in &mut self.linked_unbond_events {
                    if source == key {
                        event.validator_pub_key = display_key(*new);
                        self.events[index] = pack(event.clone());
                        *source = *new;
                    }
                }
                self.public_keys_changed = true;
            }
        }
        Ok(())
    }

    fn take_candidates(&mut self, store: &BlockStore) -> Result<stakes::Candidates> {
        match self.candidates.take() {
            Some(candidates) => Ok(candidates),
            None => stakes::Candidates::load(
                store,
                self.prior_height,
                &self.chain.retained_candidate_rows,
            ),
        }
    }

    fn pay_rewards(&mut self, store: &BlockStore, emission_open: bool) -> Result<()> {
        let mut candidates = self.take_candidates(store)?;
        let locked = if emission_open {
            Account::locked_stakes_at(
                store,
                self.prior_height,
                self.height,
                candidates.reward_owners(&self.validators),
                &self.accounts,
            )?
        } else {
            BTreeSet::new()
        };
        let (version, correct_stakes) =
            rewards::Version::at_height(self.height, &self.chain.versions);
        let excess = if correct_stakes {
            candidates.correct_v330_stakes()?
        } else {
            BigInt::ZERO
        };
        let additional = rewards::pay(
            version,
            &mut self.app,
            &mut self.validators,
            &mut candidates,
            &locked,
            &mut self.events,
        )?;
        self.chain.emission = (BigInt::from(self.chain.emission.clone()) - excess + additional)
            .to_biguint()
            .context("negative total emission")?;
        self.candidates = Some(candidates);
        Ok(())
    }

    fn update_validators(
        &mut self,
        store: &BlockStore,
        network: u8,
    ) -> Result<Vec<abci::ValidatorUpdate>> {
        let mut candidates = self.take_candidates(store)?;
        candidates.recalculate(
            &coins::View {
                store,
                height: self.prior_height,
                network,
                changes: Some(&self.coins),
            },
            &mut self.waitlist,
            &mut self.events,
        )?;
        let unlock = self.height + frozen::unbond_period(network);
        for candidate in candidates.remove_excess(&self.validators) {
            let key = display_key(candidate.key);
            self.events.push(pack(RemoveCandidateEvent {
                candidate_pub_key: key.clone(),
            }));
            for fund in candidate.into_frozen_funds() {
                self.events.push(pack(UnbondEvent {
                    address: display_address(fund.owner),
                    amount: fund.value.to_string(),
                    coin: u64::from(fund.coin),
                    validator_pub_key: key.clone(),
                }));
                self.frozen.push(store, self.prior_height, unlock, fund)?;
            }
        }
        let next = candidates.validators(&self.validators);
        let mut updates = validator_powers(&next)?;
        for old in &self.validators {
            if !next.iter().any(|validator| validator.key == old.key) {
                self.removed_validators.push(old.key);
                updates.push(validator_update(old.key, 0));
            }
        }
        self.validators = next;
        self.candidates = Some(candidates);
        Ok(updates)
    }
}

fn account_at<'a>(
    accounts: &'a mut BTreeMap<Address, Account>,
    store: &BlockStore,
    height: u64,
    address: Address,
) -> Result<&'a mut Account> {
    Ok(match accounts.entry(address) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(Account::load(store, height, address)?),
    })
}

fn genesis_metadata(chain: &mut Chain, head: minter_state::IavlHead) -> Result<Vec<u8>> {
    ensure!(
        head.height == chain.initial_height - 1,
        "genesis metadata height differs"
    );
    if let Some(cursor) = &mut chain.native_sync {
        cursor.set_genesis_root(head)?;
    }
    Ok(serde_json::to_vec(chain)?)
}

fn genesis_reply(validators: &[Validator]) -> Result<abci::ResponseInitChain> {
    Ok(abci::ResponseInitChain {
        validators: validator_powers(validators)?,
        ..Default::default()
    })
}

fn validator_powers(validators: &[Validator]) -> Result<Vec<abci::ValidatorUpdate>> {
    let total: Amount = validators.iter().map(|v| &v.stake).sum();
    validators
        .iter()
        .map(|validator| {
            let power = i64::try_from(&validator.stake * 100_000_000u32 / &total)?.max(1);
            Ok(validator_update(validator.key, power))
        })
        .collect()
}

fn validator_update(key: PublicKey, power: i64) -> abci::ValidatorUpdate {
    abci::ValidatorUpdate {
        pub_key: Some(crypto::PublicKey {
            sum: Some(public_key::Sum::Ed25519(key.to_vec())),
        }),
        power,
    }
}

fn next_gas(current: u64, times: &[i64]) -> u64 {
    let (Some(first), Some(last)) = (times.first(), times.last()) else {
        return MAX_GAS;
    };
    let delta = last.wrapping_sub(*first);
    if delta == 0 {
        return MAX_GAS;
    }
    let gas = if delta > 7 * (times.len() as i64 - 1) {
        current.wrapping_mul(7) / 10
    } else {
        current.wrapping_mul(105) / 100
    };
    gas.clamp(MIN_GAS, MAX_GAS)
}
