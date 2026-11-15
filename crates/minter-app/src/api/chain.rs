mod transaction_data;

use super::{Api, Application, Backend, ReadBudget, RpcReply, print_address, state_error};
use crate::{coins, native_sync::Submission, state::AppValues, transaction::total_gas};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use minter_api::{pb, protobuf_struct, transaction_rejected};
use minter_protocol::{Authorization, Envelope};
use minter_state::HistoricalState;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tendermint_proto::v0_34::abci;
use tendermint_rpc::{
    dialect::{DeliverTx, v0_34::Event},
    endpoint::{blockchain, broadcast, tx, tx_search, validators},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, SemaphorePermit};
use tonic::{Code, Response, Status};

// Includes JSON and base64 overhead for a Minter block and its ABCI results.
const BLOCK_RPC_LIMIT: usize = 64 * 1024 * 1024;
const SMALL_RPC_LIMIT: usize = 1024 * 1024;

pub(super) struct Rpc {
    client: reqwest::Client,
    url: String,
    calls: Semaphore,
}

impl Rpc {
    pub(super) fn new(address: SocketAddr) -> Result<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            client,
            url: format!("http://{address}/"),
            calls: Semaphore::new(8),
        })
    }

    pub(super) fn permit(&self) -> Result<SemaphorePermit<'_>, Status> {
        self.calls
            .try_acquire()
            .map_err(|_| Status::resource_exhausted("Tendermint RPC query limit reached"))
    }

    pub(super) async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
        limit: usize,
    ) -> Result<T, Status> {
        let mut response = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
            )
            .send()
            .await
            .map_err(rpc_transport)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(rpc_transport)? {
            if chunk.len() > limit - bytes.len() {
                return Err(Status::resource_exhausted(
                    "Tendermint RPC response is too large",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let reply: RpcReply<T> = serde_json::from_slice(&bytes).map_err(|error| {
            eprintln!("Minter v2 Tendermint {method} response failed: {error}");
            Status::internal("invalid Tendermint RPC response")
        })?;
        if reply.jsonrpc != "2.0" || reply.id != 1 {
            return Err(Status::internal("invalid Tendermint RPC response"));
        }
        if let Some(error) = reply.error {
            return Err(Status::failed_precondition(
                error.data().unwrap_or_else(|| error.message()),
            ));
        }
        response.error_for_status().map_err(rpc_transport)?;
        reply
            .result
            .ok_or_else(|| Status::internal("missing Tendermint RPC result"))
    }
}

fn rpc_transport(error: reqwest::Error) -> Status {
    if error.is_timeout() {
        return Status::deadline_exceeded("Tendermint RPC timed out");
    }
    eprintln!("Minter v2 Tendermint RPC failed: {error}");
    Status::unavailable("Tendermint RPC unavailable")
}

#[derive(Default)]
pub(super) struct Broadcasts {
    gate: AsyncMutex<()>,
    pending: Mutex<Option<PendingCheck>>,
}

struct PendingCheck {
    hash: [u8; 32],
    rejection: Arc<OnceLock<Rejection>>,
}

struct Rejection {
    code: u32,
    log: String,
    info: String,
}

struct TrackCheck<'a>(&'a Mutex<Option<PendingCheck>>);

impl Drop for TrackCheck<'_> {
    fn drop(&mut self) {
        self.0.lock().expect("pending check lock").take();
    }
}

impl Api {
    /// Observe the same CheckTx response returned to the companion Tendermint node.
    pub fn handle_abci(
        &self,
        app: &mut Application,
        request: abci::Request,
    ) -> Result<abci::Response> {
        let Backend::Companion { broadcasts, .. } = &self.0.backend else {
            anyhow::bail!("ABCI transport requires the companion backend");
        };
        let receipt = match &request.value {
            Some(abci::request::Value::CheckTx(check))
                if check.r#type == abci::CheckTxType::New as i32 =>
            {
                let pending = broadcasts.pending.lock().expect("pending check lock");
                pending.as_ref().and_then(|pending| {
                    (Sha256::digest(&check.tx).as_slice() == pending.hash)
                        .then(|| pending.rejection.clone())
                })
            }
            _ => None,
        };
        let response = app.handle(request)?;
        if let (Some(receipt), Some(abci::response::Value::CheckTx(check))) =
            (receipt, &response.value)
            && check.code != 0
        {
            let _ = receipt.set(Rejection {
                code: check.code,
                log: check.log.clone(),
                info: check.info.clone(),
            });
        }
        self.attach(app);
        Ok(response)
    }

    pub(super) async fn broadcast_transaction(
        &self,
        request: pb::SendTransactionRequest,
    ) -> Result<Response<pb::SendTransactionResponse>, Status> {
        let raw = request
            .tx
            .strip_prefix("0x")
            .ok_or_else(|| Status::invalid_argument("invalid transaction"))?;
        let raw = decode_hex(raw.as_bytes())?;
        let hash: [u8; 32] = Sha256::digest(&raw).into();
        if let Backend::Native { node, .. } = &self.0.backend {
            let permit =
                self.0.reads.clone().try_acquire_owned().map_err(|_| {
                    Status::resource_exhausted("transaction submission limit reached")
                })?;
            let node = node.clone();
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                node.submit(raw, None)
            });
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .map_err(|_| Status::deadline_exceeded("transaction submission timed out"))?
                .map_err(|_| Status::internal("transaction submission failed"))?
                .map_err(state_error)?;
            match result {
                Submission::Accepted => {
                    return Ok(Response::new(pb::SendTransactionResponse {
                        code: 0,
                        log: String::new(),
                        hash: format!("Mt{}", hex::encode(hash)),
                    }));
                }
                Submission::Duplicate => {
                    return Err(Status::already_exists("tx already exists in cache"));
                }
                Submission::Full => return Err(Status::resource_exhausted("mempool is full")),
                Submission::CatchingUp => return Err(Status::unavailable("node is catching up")),
                Submission::Disabled => {
                    return Err(Status::unavailable("native mempool is disabled"));
                }
                Submission::Rejected(check) => {
                    return Err(transaction_rejected(&check.log, &check.info));
                }
            }
        }
        let Backend::Companion {
            rpc, broadcasts, ..
        } = &self.0.backend
        else {
            unreachable!("the native backend has already handled this request")
        };
        let _rpc = rpc.permit()?;
        // The 0.34 broadcast RPC drops CheckTx.Info. Keep one in-flight
        // submission associated with its original ABCI receipt by raw hash.
        let _gate = tokio::time::timeout(Duration::from_secs(5), broadcasts.gate.lock())
            .await
            .map_err(|_| Status::deadline_exceeded("Tendermint broadcast timed out"))?;
        let receipt = Arc::new(OnceLock::new());
        *broadcasts.pending.lock().expect("pending check lock") = Some(PendingCheck {
            hash,
            rejection: receipt.clone(),
        });
        let _tracking = TrackCheck(&broadcasts.pending);

        let result: broadcast::tx_sync::Response = rpc
            .call(
                "broadcast_tx_sync",
                json!({"tx": BASE64.encode(raw)}),
                SMALL_RPC_LIMIT,
            )
            .await
            .map_err(|error| {
                if error.code() == Code::FailedPrecondition
                    && error.message() == "tx already exists in cache"
                {
                    Status::already_exists(error.message())
                } else {
                    error
                }
            })?;
        if result.hash.as_bytes() != hash {
            return Err(Status::internal(
                "Tendermint returned the wrong transaction hash",
            ));
        }
        let code = u32::from(result.code);
        if code != 0 {
            let info = receipt
                .get()
                .filter(|rejected| rejected.code == code && rejected.log == result.log)
                .map_or("", |rejected| rejected.info.as_str());
            return Err(transaction_rejected(&result.log, info));
        }
        Ok(Response::new(pb::SendTransactionResponse {
            code: 0,
            log: result.log,
            hash: format!("Mt{}", hex::encode(hash)),
        }))
    }

    pub(super) async fn read_transaction(
        &self,
        request: pb::TransactionRequest,
    ) -> Result<Response<pb::TransactionResponse>, Status> {
        let raw = request.hash.as_bytes();
        if raw.len() < 3 {
            return Err(Status::invalid_argument("invalid hash"));
        }
        let hash = decode_hex(&raw[2..])?;
        let rpc = self.rpc()?;
        let _rpc = rpc.permit()?;
        let tx: tx::v0_34::DialectResponse = rpc
            .call(
                "tx",
                json!({"hash": BASE64.encode(&hash), "prove": false}),
                SMALL_RPC_LIMIT,
            )
            .await?;
        if tx.hash.as_bytes() != hash || tx.hash.as_bytes() != Sha256::digest(&tx.tx).as_slice() {
            return Err(Status::internal(
                "Tendermint returned the wrong transaction",
            ));
        }
        self.read(0, move |state, source, budget| {
            TxEncoder::new(state, source.network, budget)
                .encode(&tx.tx, tx.height.value(), u64::from(tx.index), tx.tx_result)
                .map_err(state_error)
        })
        .await
    }

    pub(super) async fn search_transactions(
        &self,
        request: pb::TransactionsRequest,
    ) -> Result<Response<pb::TransactionsResponse>, Status> {
        let rpc = self.rpc()?;
        let _rpc = rpc.permit()?;
        let found: tx_search::v0_34::DialectResponse = rpc
            .call(
                "tx_search",
                json!({"query": request.query, "prove": false, "page": request.page.to_string(),
                   "per_page": request.per_page.to_string(), "order_by": "desc"}),
                BLOCK_RPC_LIMIT,
            )
            .await?;
        if found.txs.is_empty() {
            return Ok(Response::new(pb::TransactionsResponse::default()));
        }
        self.read(0, move |state, source, budget| {
            let mut encoder = TxEncoder::new(state, source.network, budget);
            let mut transactions = Vec::with_capacity(found.txs.len());
            for tx in found.txs {
                let mut value = encoder
                    .encode(&tx.tx, tx.height.value(), u64::from(tx.index), tx.tx_result)
                    .map_err(state_error)?;
                // The Go search response leaves service_data at its protobuf default.
                value.service_data.clear();
                transactions.push(value);
            }
            Ok(pb::TransactionsResponse { transactions })
        })
        .await
    }

    pub(super) async fn read_block(
        &self,
        request: pb::BlockRequest,
    ) -> Result<Response<pb::BlockResponse>, Status> {
        use pb::BlockField::*;
        let rpc = self.rpc()?;
        let _rpc = rpc.permit()?;
        let height = request.height;
        let height_param = json!({"height": (height as i64).to_string()});
        let result: RpcBlock = rpc
            .call("block", height_param.clone(), BLOCK_RPC_LIMIT)
            .await
            .map_err(|error| not_found(error, "Block not found"))?;
        let block = result
            .block
            .ok_or_else(|| Status::not_found("Block not found"))?;
        if block.header.height.parse() != Ok(height) {
            return Err(Status::internal(
                "Tendermint returned the wrong block height",
            ));
        }
        let selected =
            |field| request.fields.is_empty() || request.fields.contains(&(field as i32));
        let txs = block.data.txs.unwrap_or_default();
        let mut response = pb::BlockResponse {
            hash: result.block_id.hash.to_ascii_lowercase(),
            height,
            time: block.header.time,
            transaction_count: txs.len() as u64,
            ..Default::default()
        };
        if request.events && self.0.source.get().is_some() {
            response.events = self
                .read(0, move |state, _, _| {
                    Ok(super::events_at(state, height)?.unwrap_or_default())
                })
                .await?
                .into_inner();
        }

        if selected(Transactions) {
            let results: BlockResults = rpc
                .call("block_results", height_param, BLOCK_RPC_LIMIT)
                .await
                .map_err(|error| not_found(error, "Block results not found"))?;
            if results.height.parse() != Ok(height) {
                return Err(Status::internal(
                    "Tendermint returned the wrong block results",
                ));
            }
            let results = results.txs_results.unwrap_or_default();
            if results.len() != txs.len() {
                return Err(Status::internal(
                    "block transaction results do not match the block",
                ));
            }
            if !txs.is_empty() {
                response.transactions = self
                    .read(0, move |state, source, budget| {
                        let mut encoder = TxEncoder::new(state, source.network, budget);
                        let mut transactions = Vec::with_capacity(txs.len());
                        for (index, (tx, result)) in txs.into_iter().zip(results).enumerate() {
                            if result.code.is_ok() || request.failed_txs {
                                let raw = BASE64
                                    .decode(tx)
                                    .context("invalid Tendermint transaction")
                                    .map_err(state_error)?;
                                transactions.push(
                                    encoder
                                        .encode(&raw, height, index as u64, result)
                                        .map_err(state_error)?,
                                );
                            }
                        }
                        Ok(transactions)
                    })
                    .await?
                    .into_inner();
            }
        }
        if selected(Size) {
            let height = height.to_string();
            let metadata: blockchain::Response = rpc
                .call(
                    "blockchain",
                    json!({"minHeight": height, "maxHeight": height}),
                    SMALL_RPC_LIMIT,
                )
                .await
                .map_err(|error| not_found(error, "Block not found"))?;
            let meta = metadata
                .block_metas
                .first()
                .filter(|meta| {
                    meta.block_id
                        .hash
                        .to_string()
                        .eq_ignore_ascii_case(&response.hash)
                })
                .ok_or_else(|| Status::internal("block metadata does not match the block"))?;
            response.size = u64::try_from(meta.block_size)
                .map_err(|_| Status::internal("invalid Tendermint block size"))?;
        }
        if selected(Proposer) || selected(Validators) {
            let set: validators::Response = rpc.call(
                "validators",
                json!({"height": height.saturating_sub(1).max(1).to_string(), "page": "1", "per_page": "100"}),
                SMALL_RPC_LIMIT,
            ).await.map_err(|error| {
                if error.code() == Code::FailedPrecondition {
                    Status::internal(error.message())
                } else {
                    error
                }
            })?;
            for validator in set.validators {
                let address = validator.address.to_string();
                let public_key = format!("Mp{}", hex::encode(validator.pub_key.to_bytes()));
                if selected(Proposer)
                    && address.eq_ignore_ascii_case(&block.header.proposer_address)
                {
                    response.proposer.clone_from(&public_key);
                }
                if selected(Validators) {
                    let signed = block
                        .last_commit
                        .as_ref()
                        .and_then(|commit| commit.signatures.as_ref())
                        .is_some_and(|signatures| {
                            signatures
                                .iter()
                                .any(|sig| address.eq_ignore_ascii_case(&sig.validator_address))
                        });
                    response
                        .validators
                        .push(pb::block_response::Validator { public_key, signed });
                }
            }
        }
        if selected(Evidence) {
            let evidence = block
                .evidence
                .evidence
                .unwrap_or_default()
                .into_iter()
                .map(protobuf_struct)
                .collect::<Result<Vec<_>, _>>()?;
            response.evidence = Some(pb::block_response::Evidence { evidence });
        }
        if selected(BlockReward) {
            let v3 = self.0.source.get().and_then(|source| source.v3_height);
            if v3.is_some_and(|v3| height >= v3) {
                match self
                    .read(height, |state, _, _| {
                        AppValues::load_from(|key| state.value(key)).map_err(state_error)
                    })
                    .await
                {
                    Ok(value) => {
                        let values = value.into_inner();
                        response.block_reward = Some(values.reward.to_string());
                        response.locked_stake_rewards =
                            Some((values.safe_reward * 3u32).to_string());
                    }
                    Err(error) if error.code() == Code::NotFound => {}
                    Err(error) => return Err(error),
                }
            } else {
                response.block_reward = Some(legacy_reward(height).to_string());
            }
        }
        Ok(Response::new(response))
    }
}

fn not_found(error: Status, message: &'static str) -> Status {
    if error.code() == Code::FailedPrecondition {
        Status::not_found(message)
    } else {
        error
    }
}

fn legacy_reward(height: u64) -> u128 {
    // coreV2/rewards before tokenomics v300.
    let height = height.saturating_add(9_150_000);
    let bip = match height.cmp(&43_702_611) {
        std::cmp::Ordering::Greater => 0,
        std::cmp::Ordering::Equal => 68,
        std::cmp::Ordering::Less => (333 - height / 200_000).max(1),
    };
    u128::from(bip) * 1_000_000_000_000_000_000
}

fn decode_hex(bytes: &[u8]) -> Result<Vec<u8>, Status> {
    // Go's encoding/hex reports the first invalid byte before odd length.
    if let Some(byte) = bytes.iter().copied().find(|byte| !byte.is_ascii_hexdigit()) {
        let ch = char::from(byte);
        let suffix = if ch == ' ' || ch.is_ascii_graphic() || ch.is_alphanumeric() {
            format!(" '{ch}'")
        } else {
            String::new()
        };
        return Err(Status::invalid_argument(format!(
            "encoding/hex: invalid byte: U+{byte:04X}{suffix}"
        )));
    }
    hex::decode(bytes).map_err(|_| Status::invalid_argument("encoding/hex: odd length hex string"))
}

#[derive(Deserialize)]
struct RpcBlock {
    block_id: BlockId,
    block: Option<Block>,
}

#[derive(Deserialize)]
struct BlockId {
    hash: String,
}

#[derive(Deserialize)]
struct Block {
    header: BlockHeader,
    data: BlockData,
    evidence: BlockEvidence,
    last_commit: Option<Commit>,
}

#[derive(Deserialize)]
struct BlockHeader {
    height: String,
    time: String,
    proposer_address: String,
}

#[derive(Deserialize)]
struct BlockData {
    txs: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BlockResults {
    height: String,
    txs_results: Option<Vec<DeliverTx<Event>>>,
}

#[derive(Deserialize)]
struct BlockEvidence {
    evidence: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Commit {
    signatures: Option<Vec<CommitSignature>>,
}

#[derive(Deserialize)]
struct CommitSignature {
    #[serde(default)]
    validator_address: String,
}

struct TxEncoder<'a, 'db> {
    state: &'a HistoricalState<'db>,
    network: u8,
    budget: &'a ReadBudget,
    symbols: BTreeMap<u32, String>,
}

impl<'a, 'db> TxEncoder<'a, 'db> {
    fn new(state: &'a HistoricalState<'db>, network: u8, budget: &'a ReadBudget) -> Self {
        Self {
            state,
            network,
            budget,
            symbols: BTreeMap::new(),
        }
    }

    fn coin(&mut self, id: u32) -> Result<pb::Coin> {
        self.budget.check()?;
        let symbol = match self.symbols.entry(id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let symbol = if id == 0 {
                    coins::Symbol::base(self.network).name().into_owned()
                } else {
                    coins::Coin::load_from(id, |key| self.state.value(key))?
                        .with_context(|| {
                            format!("indexed transaction refers to missing coin {id}")
                        })?
                        .full_symbol()
                };
                entry.insert(symbol)
            }
        };
        Ok(pb::Coin {
            id: u64::from(id),
            symbol: symbol.clone(),
        })
    }

    fn encode(
        &mut self,
        raw: &[u8],
        height: u64,
        index: u64,
        result: DeliverTx<Event>,
    ) -> Result<pb::TransactionResponse> {
        self.budget.check()?;
        let envelope = Envelope::decode(raw)?;
        let fields = envelope.fields();
        let (sender, signatures) = match envelope.authorization() {
            Authorization::Single(signature) => (
                signature
                    .recover(envelope.signing_hash())
                    .unwrap_or_default(),
                0,
            ),
            Authorization::Multisig {
                account,
                signatures,
            } => (*account, signatures.len()),
        };
        let mut first_coin = None;
        let (data, kind_gas) = transaction_data::decode(fields.kind, fields.data, |id| {
            first_coin.get_or_insert(id);
            self.coin(id)
        })?;
        let commission_coin = if matches!(fields.kind, 0x03 | 0x19) {
            first_coin.unwrap_or_default()
        } else {
            fields.gas_coin
        };
        let mut tags = BTreeMap::new();
        if let Some(event) = result.events.into_iter().next() {
            for attribute in event.attributes {
                tags.insert(
                    String::from_utf8(attribute.key)?,
                    String::from_utf8(attribute.value)?,
                );
            }
        }
        let gas = total_gas(fields, commission_coin, kind_gas, signatures);
        ensure!(gas >= 0, "negative gas for indexed transaction");
        Ok(pb::TransactionResponse {
            hash: format!("Mt{}", hex::encode(Sha256::digest(raw))),
            raw_tx: hex::encode(raw),
            height,
            index,
            from: print_address(sender),
            nonce: fields.nonce,
            gas: gas as u64,
            gas_price: u64::from(fields.gas_price),
            gas_coin: Some(self.coin(fields.gas_coin)?),
            type_hex: format!("0x{:02x}", fields.kind),
            r#type: u64::from(fields.kind),
            data: Some(data),
            payload: fields.payload.to_vec(),
            service_data: fields.service_data.to_vec(),
            tags: tags.into_iter().collect(),
            code: u64::from(u32::from(result.code)),
            log: result.log,
        })
    }
}
