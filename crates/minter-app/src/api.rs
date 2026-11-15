mod chain;
mod subscriptions;

use crate::{
    Application, TOTAL_EMISSION, Upgrade, bancor, coins,
    commission::Fees,
    halts,
    native_sync::{NativeBlocks, NativeNode},
    stakes,
    state::{Account, Amount, AppValues, display_key},
    updates::{self, CommissionVotes, NetworkVotes},
};
use anyhow::{Context, Result};
use minter_api::{
    Any, coin_not_found, decode_events, event_struct,
    pb::{
        AddressBalance, AddressDelegatedBalance, AddressRequest, AddressResponse, BlockRequest,
        BlockResponse, Coin, CoinIdRequest, CoinInfoRequest, CoinInfoResponse,
        CommissionVotesRequest, CommissionVotesResponse, EventsRequest, EventsResponse,
        HaltsRequest, HaltsResponse, Multisig, PriceCommissionRequest, PriceCommissionResponse,
        SendTransactionRequest, SendTransactionResponse, StatusResponse, SubscribeRequest,
        SubscribeResponse, TransactionRequest, TransactionResponse, TransactionsRequest,
        TransactionsResponse, UpdateVotesRequest, UpdateVotesResponse, VersionNetworkRequest,
        VersionNetworkResponse, api_service_server::ApiService, commission_votes_response,
        update_votes_response, version_network_response,
    },
};
use minter_protocol::Address;
use minter_state::{HistoricalState, StateReader, UnavailableHeight};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet, btree_map::Entry},
    net::SocketAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tendermint_rpc::{ResponseError, endpoint::status};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::sync::Semaphore;
use tonic::{Request, Response, Status, codegen::BoxStream};

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Api(Arc<Shared>);

struct Shared {
    source: OnceLock<Source>,
    reads: Arc<Semaphore>,
    backend: Backend,
}

enum Backend {
    Companion {
        rpc: chain::Rpc,
        broadcasts: chain::Broadcasts,
        subscriptions: subscriptions::Subscriptions,
    },
    Native {
        node: NativeNode,
        id: tendermint::node::Id,
        public_key: String,
    },
}

#[derive(Deserialize)]
struct RpcReply<T> {
    jsonrpc: String,
    id: u64,
    result: Option<T>,
    error: Option<ResponseError>,
}

struct Source {
    reader: StateReader,
    network: u8,
    chain_id: String,
    initial_height: u64,
    v3_height: Option<u64>,
    checkpoint_emission: Amount,
    checkpoint_versions: Vec<Upgrade>,
}

struct ReadBudget {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl ReadBudget {
    fn check(&self) -> Result<(), Status> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(Status::cancelled("state query cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(Status::deadline_exceeded("state query timed out"));
        }
        Ok(())
    }
}

struct CancelRead(Arc<AtomicBool>);

impl Drop for CancelRead {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Api {
    pub fn new(tendermint_rpc: SocketAddr) -> Result<Self> {
        Ok(Self::with_backend(Backend::Companion {
            rpc: chain::Rpc::new(tendermint_rpc)?,
            broadcasts: chain::Broadcasts::default(),
            subscriptions: subscriptions::Subscriptions::new(tendermint_rpc),
        }))
    }

    pub fn native(
        node: NativeNode,
        id: tendermint::node::Id,
        public_key: [u8; 32],
    ) -> Result<Self> {
        let api = Self::with_backend(Backend::Native {
            node: node.clone(),
            id,
            public_key: format!("Mp{}", hex::encode(public_key)),
        });
        api.attach(node.lock()?.application());
        Ok(api)
    }

    fn with_backend(backend: Backend) -> Self {
        Self(Arc::new(Shared {
            source: OnceLock::new(),
            reads: Arc::new(Semaphore::new(8)),
            backend,
        }))
    }

    fn rpc(&self) -> Result<&chain::Rpc, Status> {
        match &self.0.backend {
            Backend::Companion { rpc, .. } => Ok(rpc),
            Backend::Native { .. } => Err(Status::unimplemented(
                "this method needs the companion Tendermint RPC",
            )),
        }
    }

    pub fn shutdown_subscriptions(&self) {
        if let Backend::Companion { subscriptions, .. } = &self.0.backend {
            subscriptions.shutdown();
        }
    }

    /// Publish the store once genesis or State Sync has installed a committed root.
    pub fn attach(&self, app: &Application) {
        if self.0.source.get().is_none()
            && let (Some(reader), Some(chain)) = (app.store.reader(), &app.chain)
        {
            let _ = self.0.source.set(Source {
                reader,
                network: chain.transaction_chain_id,
                chain_id: chain.id.clone(),
                initial_height: chain.initial_height,
                v3_height: chain
                    .versions
                    .iter()
                    .find(|version| version.name == "v300")
                    .map(|version| version.height + 1),
                checkpoint_emission: chain.emission.clone(),
                checkpoint_versions: chain.versions.clone(),
            });
        }
    }

    async fn read<T: Send + 'static>(
        &self,
        height: u64,
        run: impl FnOnce(&HistoricalState<'_>, &Source, &ReadBudget) -> Result<T, Status>
        + Send
        + 'static,
    ) -> Result<Response<T>, Status> {
        let permit = self
            .0
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("state query limit reached"))?;
        let shared = self.0.clone();
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let budget = ReadBudget {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
        };
        let _cancel = CancelRead(budget.cancelled.clone());
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            budget.check()?;
            let source = shared
                .source
                .get()
                .ok_or_else(|| Status::unavailable("state is not initialized"))?;
            let state = if height == 0 {
                source.reader.latest()
            } else {
                source.reader.at_version(height)
            }
            .map_err(state_error)?;
            run(&state, source, &budget)
        });
        let result = tokio::time::timeout_at(deadline.into(), task)
            .await
            .map_err(|_| Status::deadline_exceeded("state query timed out"))?
            .map_err(|_| Status::internal("state query failed"))??;
        Ok(Response::new(result))
    }
}

#[tonic::async_trait]
impl ApiService for Api {
    async fn price_commission(
        &self,
        request: Request<PriceCommissionRequest>,
    ) -> Result<Response<PriceCommissionResponse>, Status> {
        self.read(request.into_inner().height, |state, source, _| {
            let raw = state
                .value(b"p")
                .and_then(|value| value.context("missing commission prices"))
                .map_err(state_error)?;
            commission_response(state, source.network, &raw).map_err(state_error)
        })
        .await
    }

    async fn commission_votes(
        &self,
        request: Request<CommissionVotesRequest>,
    ) -> Result<Response<CommissionVotesResponse>, Status> {
        let CommissionVotesRequest {
            height,
            target_version,
        } = request.into_inner();
        self.read(height, move |state, source, budget| {
            let key = CommissionVotes::key(target_version);
            let proposals = read_proposals(state, &key).map_err(state_error)?;
            let mut votes = Vec::with_capacity(proposals.len());
            for proposal in proposals {
                budget.check()?;
                votes.push(commission_votes_response::Vote {
                    price: Some(
                        commission_response(state, source.network, &proposal.payload)
                            .map_err(state_error)?,
                    ),
                    public_keys: proposal.voters.into_iter().map(display_key).collect(),
                });
            }
            Ok(CommissionVotesResponse { votes })
        })
        .await
    }

    async fn update_votes(
        &self,
        request: Request<UpdateVotesRequest>,
    ) -> Result<Response<UpdateVotesResponse>, Status> {
        let UpdateVotesRequest {
            height,
            target_version,
        } = request.into_inner();
        self.read(height, move |state, _, budget| {
            let key = NetworkVotes::key(target_version);
            let proposals = read_proposals(state, &key).map_err(state_error)?;
            let mut votes = Vec::with_capacity(proposals.len());
            for proposal in proposals {
                budget.check()?;
                votes.push(update_votes_response::Vote {
                    version: String::from_utf8(proposal.payload)
                        .context("invalid network version name")
                        .map_err(state_error)?,
                    public_keys: proposal.voters.into_iter().map(display_key).collect(),
                });
            }
            Ok(UpdateVotesResponse { votes })
        })
        .await
    }

    async fn halts(
        &self,
        request: Request<HaltsRequest>,
    ) -> Result<Response<HaltsResponse>, Status> {
        let key = halts::key(request.into_inner().height);
        self.read(0, move |state, _, _| {
            let voters = state
                .value(&key)
                .and_then(|value| value.as_deref().map(halts::decode_voters).transpose())
                .map_err(state_error)?
                .unwrap_or_default();
            Ok(HaltsResponse {
                public_keys: voters.into_iter().map(display_key).collect(),
            })
        })
        .await
    }

    async fn version_network(
        &self,
        _: Request<VersionNetworkRequest>,
    ) -> Result<Response<VersionNetworkResponse>, Status> {
        if self.0.source.get().is_none() {
            return Ok(Response::new(VersionNetworkResponse::default()));
        }
        self.read(0, |state, source, _| {
            #[derive(Deserialize)]
            struct Versions {
                #[serde(default)]
                versions: Vec<Upgrade>,
            }
            let raw = state.execution_metadata().map_err(state_error)?;
            let committed = raw
                .as_deref()
                .map(serde_json::from_slice::<Versions>)
                .transpose()
                .context("decode committed upgrade history")
                .map_err(state_error)?;
            let versions = committed
                .as_ref()
                .map(|data| data.versions.as_slice())
                .filter(|versions| !versions.is_empty())
                .unwrap_or(&source.checkpoint_versions);
            Ok(VersionNetworkResponse {
                current: versions
                    .last()
                    .map_or_else(String::new, |version| version.name.clone()),
                versions: versions
                    .iter()
                    .map(|version| version_network_response::Version {
                        name: version.name.clone(),
                        height: version.height,
                    })
                    .collect(),
            })
        })
        .await
    }

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<BoxStream<SubscribeResponse>>, Status> {
        let Backend::Companion { subscriptions, .. } = &self.0.backend else {
            return Err(Status::unimplemented(
                "native subscriptions are unavailable",
            ));
        };
        subscriptions
            .open(request.into_inner().query)
            .await
            .map(Response::new)
    }

    async fn status(&self, _: Request<()>) -> Result<Response<StatusResponse>, Status> {
        if let Backend::Native {
            node,
            id,
            public_key,
        } = &self.0.backend
        {
            let node = node.clone();
            let id = id.to_string();
            let public_key = public_key.clone();
            return self
                .read(0, move |state, source, _| {
                    let height = state.head().height;
                    let peer_height = node.lock().map_err(state_error)?.peer_height();
                    let block = NativeBlocks::read(&source.reader, height).map_err(state_error)?;
                    let (hash, app_hash, time) = block.map_or_else(
                        || (String::new(), String::new(), 0),
                        |block| {
                            (
                                block.id().hash.to_string(),
                                hex::encode_upper(block.block().header.app_hash.as_bytes()),
                                block.block().header.time.unix_timestamp_nanos(),
                            )
                        },
                    );
                    status_response(
                        state,
                        source,
                        StatusResponse {
                            latest_block_hash: hash,
                            latest_app_hash: app_hash,
                            latest_block_height: height,
                            latest_block_time: status_time(time).map_err(state_error)?,
                            catching_up: peer_height == 0 || peer_height > height.saturating_add(2),
                            public_key,
                            node_id: id,
                            moniker: "minter-rust".into(),
                            ..Default::default()
                        },
                    )
                })
                .await;
        }
        let rpc = self.rpc()?;
        let _rpc = rpc.permit()?;
        let node: status::Response = rpc
            .call("status", serde_json::json!({}), 1024 * 1024)
            .await
            .map_err(|error| {
                eprintln!("Minter v2 status RPC failed: {error:#}");
                Status::internal("Tendermint status unavailable")
            })?;
        self.read(0, move |state, source, _| {
            if node.node_info.network.as_str() != source.chain_id {
                return Err(Status::internal(
                    "Tendermint network differs from Minter state",
                ));
            }
            status_response(
                state,
                source,
                StatusResponse {
                    latest_block_hash: node.sync_info.latest_block_hash.to_string(),
                    latest_app_hash: hex::encode_upper(node.sync_info.latest_app_hash.as_bytes()),
                    latest_block_height: node.sync_info.latest_block_height.value(),
                    latest_block_time: status_time(
                        node.sync_info.latest_block_time.unix_timestamp_nanos(),
                    )
                    .map_err(state_error)?,
                    catching_up: node.sync_info.catching_up,
                    public_key: format!(
                        "Mp{}",
                        hex::encode(node.validator_info.pub_key.to_bytes())
                    ),
                    node_id: node.node_info.id.to_string(),
                    moniker: node.node_info.moniker.to_string(),
                    ..Default::default()
                },
            )
        })
        .await
    }

    async fn address(
        &self,
        request: Request<AddressRequest>,
    ) -> Result<Response<AddressResponse>, Status> {
        let request = request.into_inner();
        let address = parse_address(&request.address)
            .ok_or_else(|| Status::invalid_argument("invalid address"))?;
        self.read(request.height, move |state, source, budget| {
            address_response(
                state,
                source.network,
                address,
                request.delegated,
                request.height == 0,
                budget,
            )
            .map_err(state_error)
        })
        .await
    }

    async fn coin_info(
        &self,
        request: Request<CoinInfoRequest>,
    ) -> Result<Response<CoinInfoResponse>, Status> {
        let request = request.into_inner();
        self.read(request.height, move |state, source, budget| {
            let asset = coin_by_symbol(state, source.network, &request.symbol, budget)
                .map_err(state_error)?
                .ok_or_else(|| coin_not_found("coin_symbol", request.symbol))?;
            asset.info(state).map_err(state_error)
        })
        .await
    }

    async fn coin_info_by_id(
        &self,
        request: Request<CoinIdRequest>,
    ) -> Result<Response<CoinInfoResponse>, Status> {
        let request = request.into_inner();
        self.read(request.height, move |state, source, _| {
            // The Go API accepts uint64 and converts it to the chain's uint32 CoinID.
            let asset = Asset::load(state, source.network, request.id as u32)
                .map_err(state_error)?
                .ok_or_else(|| coin_not_found("coin_id", (request.id as i64).to_string()))?;
            asset.info(state).map_err(state_error)
        })
        .await
    }

    async fn block(
        &self,
        request: Request<BlockRequest>,
    ) -> Result<Response<BlockResponse>, Status> {
        self.read_block(request.into_inner()).await
    }

    async fn events(
        &self,
        request: Request<EventsRequest>,
    ) -> Result<Response<EventsResponse>, Status> {
        let EventsRequest { height, search } = request.into_inner();
        let missing = move || Status::not_found(format!("version {height} doesn't exist yet"));
        if self.0.source.get().is_none() {
            return Err(missing());
        }
        self.read(0, move |state, _, budget| {
            let batch = events_at(state, height)?.ok_or_else(missing)?;
            let search: HashSet<_> = search.iter().map(String::as_str).collect();
            let mut events = Vec::new();
            for event in batch {
                budget.check()?;
                if let Some(event) = event_struct(event, &search)? {
                    events.push(event);
                }
            }
            Ok(EventsResponse { events })
        })
        .await
    }

    async fn transaction(
        &self,
        request: Request<TransactionRequest>,
    ) -> Result<Response<TransactionResponse>, Status> {
        self.read_transaction(request.into_inner()).await
    }

    async fn transactions(
        &self,
        request: Request<TransactionsRequest>,
    ) -> Result<Response<TransactionsResponse>, Status> {
        self.search_transactions(request.into_inner()).await
    }

    async fn send_transaction(
        &self,
        request: Request<SendTransactionRequest>,
    ) -> Result<Response<SendTransactionResponse>, Status> {
        self.broadcast_transaction(request.into_inner()).await
    }
}

fn status_response(
    state: &HistoricalState<'_>,
    source: &Source,
    mut reply: StatusResponse,
) -> Result<StatusResponse, Status> {
    #[derive(Deserialize)]
    struct Emission {
        emission: Amount,
    }
    let emission = match state.execution_metadata().map_err(state_error)? {
        Some(raw) => {
            serde_json::from_slice::<Emission>(&raw)
                .context("decode committed emission")
                .map_err(state_error)?
                .emission
        }
        None => source.checkpoint_emission.clone(),
    };
    let app = AppValues::load_from(|key| state.value(key)).map_err(state_error)?;
    reply.version = concat!("v", env!("CARGO_PKG_VERSION")).into();
    reply.network.clone_from(&source.chain_id);
    reply.initial_height = source.initial_height;
    reply.keep_last_states = state.keep_iavl_versions().map_err(state_error)?;
    reply.total_slashed = app.slashed.to_string();
    reply.current_emission = emission.to_string();
    Ok(reply)
}

fn read_proposals(state: &HistoricalState<'_>, key: &[u8]) -> Result<Vec<updates::Proposal>> {
    Ok(state
        .value(key)?
        .as_deref()
        .map(updates::decode_proposals)
        .transpose()?
        .unwrap_or_default())
}

fn commission_response(
    state: &HistoricalState<'_>,
    network: u8,
    raw: &[u8],
) -> Result<PriceCommissionResponse> {
    let fees = Fees::decode(raw)?;
    let symbol = if fees.price_coin == 0 {
        coins::Symbol::base(network).name().into_owned()
    } else {
        coins::Coin::load_from(fees.price_coin, |key| state.value(key))?
            .context("missing commission price coin")?
            .full_symbol()
    };
    Ok(fees.response(symbol))
}

fn events_at(state: &HistoricalState<'_>, height: u64) -> Result<Option<Vec<Any>>, Status> {
    // The Go API uses a 32-bit height for both Events and Block.events.
    state
        .block_events(u64::from(height as u32))
        .and_then(|bytes| bytes.map(|bytes| Ok(decode_events(&bytes)?)).transpose())
        .map_err(state_error)
}

fn status_time(nanos: i128) -> Result<String> {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(nanos)?;
    // Go formats time.Unix(...).Format(time.RFC3339Nano) in the local zone.
    let offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    Ok(utc.to_offset(offset).format(&Rfc3339)?)
}

fn state_error(error: anyhow::Error) -> Status {
    let error = match error.downcast::<Status>() {
        Ok(status) => return status,
        Err(error) => error,
    };
    if error.is::<UnavailableHeight>() {
        return Status::not_found("version does not exist");
    }
    eprintln!("Minter v2 state query failed: {error:#}");
    Status::internal("state query failed")
}

fn parse_address(input: &str) -> Option<Address> {
    let value = input
        .strip_prefix("Mx")
        .or_else(|| input.strip_prefix("mx"))?;
    let bytes = hex::decode(value).ok()?;
    let bytes = &bytes[bytes.len().saturating_sub(20)..];
    let mut address = [0; 20];
    address[20 - bytes.len()..].copy_from_slice(bytes);
    Some(address)
}

fn print_address(address: Address) -> String {
    format!("Mx{}", hex::encode(address))
}

fn coin_by_symbol(
    state: &HistoricalState<'_>,
    network: u8,
    input: &str,
    budget: &ReadBudget,
) -> Result<Option<Asset>> {
    if input.is_empty() || input.ends_with('-') {
        return Ok(None);
    }
    let base = input
        .split_once('-')
        .filter(|(base, _)| base.len() > 3)
        .map_or(input, |(base, _)| base);
    let mut symbol = coins::Symbol([0; 10]);
    let len = base.len().min(symbol.0.len());
    symbol.0[..len].copy_from_slice(&base.as_bytes()[..len]);
    if symbol == coins::Symbol::base(network) {
        return Asset::load(state, network, 0);
    }
    let mut parts = input.split('-');
    let prefix = parts.next().unwrap_or_default();
    let version = if prefix.len() >= 3 {
        parts.next().map_or(0, ticker_version)
    } else {
        0
    };
    let ids = coins::ticker_ids(symbol, |key| state.value(key))?.unwrap_or_default();
    for id in ids {
        budget.check()?;
        let coin = coins::Coin::load_from(id, |key| state.value(key))?
            .context("ticker refers to a missing coin")?;
        if coin.version == version {
            return Ok(Some(Asset::from_coin(state, id, coin)?));
        }
    }
    Ok(None)
}

fn ticker_version(text: &str) -> u16 {
    let mut value = 0u16;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return 0;
        }
        match value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u16::from(byte - b'0')))
        {
            Some(next) => value = next,
            None => return u16::MAX,
        }
    }
    value
}

struct Asset {
    id: u32,
    coin: coins::Coin,
    supply: coins::Supply,
}

impl Asset {
    fn load(state: &HistoricalState<'_>, network: u8, id: u32) -> Result<Option<Self>> {
        if id == 0 {
            return Ok(Some(Self {
                id,
                coin: coins::Coin {
                    name: Vec::new(),
                    ratio: 0,
                    max_supply: Amount::from(TOTAL_EMISSION),
                    version: 0,
                    symbol: coins::Symbol::base(network),
                    mintable: false,
                    burnable: false,
                },
                supply: coins::Supply {
                    volume: Amount::ZERO,
                    reserve: Amount::ZERO,
                },
            }));
        }
        coins::Coin::load_from(id, |key| state.value(key))?
            .map(|coin| Self::from_coin(state, id, coin))
            .transpose()
    }

    fn from_coin(state: &HistoricalState<'_>, id: u32, coin: coins::Coin) -> Result<Self> {
        Ok(Self {
            id,
            coin,
            supply: coins::load_supply(id, |key| state.value(key))?,
        })
    }

    fn info(self, state: &HistoricalState<'_>) -> Result<CoinInfoResponse> {
        let symbol = self.coin.full_symbol();
        let owner = coins::load_owner(self.coin.symbol, |key| state.value(key))?;
        Ok(CoinInfoResponse {
            id: u64::from(self.id),
            symbol,
            name: String::from_utf8(self.coin.name).context("invalid coin name")?,
            volume: self.supply.volume.to_string(),
            crr: u64::from(self.coin.ratio),
            reserve_balance: self.supply.reserve.to_string(),
            max_supply: self.coin.max_supply.to_string(),
            owner_address: owner.map(print_address),
            mintable: self.coin.mintable,
            burnable: self.coin.burnable,
        })
    }

    fn reference(&self) -> Coin {
        Coin {
            id: u64::from(self.id),
            symbol: self.coin.full_symbol(),
        }
    }

    fn bip_value(&self, value: &Amount) -> Result<Amount> {
        if self.id == 0 {
            return Ok(value.clone());
        }
        if self.coin.ratio == 0 {
            return Ok(Amount::ZERO);
        }
        bancor::sale_return(
            &self.supply.volume,
            &self.supply.reserve,
            self.coin.ratio,
            value,
        )
    }

    fn balance(&self, value: &Amount, bip_value: &Amount) -> AddressBalance {
        AddressBalance {
            coin: Some(self.reference()),
            value: value.to_string(),
            bip_value: bip_value.to_string(),
        }
    }
}

fn address_response(
    state: &HistoricalState<'_>,
    network: u8,
    address: Address,
    delegated: bool,
    current: bool,
    budget: &ReadBudget,
) -> Result<AddressResponse> {
    let read = |key: &[u8]| -> Result<Option<Vec<u8>>> {
        budget.check()?;
        state.value(key)
    };
    let account = Account::load_from(address, read)?;
    let balances = account.stored_balances(read)?;
    let stakes = if delegated {
        let retained = if current {
            state
                .execution_metadata()?
                .map(|raw| serde_json::from_slice::<crate::Chain>(&raw))
                .transpose()?
                .map(|chain| chain.retained_candidate_rows)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        stakes::delegated(state, address, &retained, || Ok(budget.check()?))?
    } else {
        BTreeMap::new()
    };
    let mut assets = BTreeMap::new();
    for id in balances
        .iter()
        .map(|(id, _)| *id)
        .chain(stakes.keys().copied())
    {
        budget.check()?;
        if let Entry::Vacant(entry) = assets.entry(id) {
            entry.insert(
                Asset::load(state, network, id)?.context("account refers to a missing coin")?,
            );
        }
    }
    let mut result = AddressResponse {
        transaction_count: account.nonce,
        locked_stake_until_block: account.lock_stake_until,
        multisig: (!account.multisig.weights.is_empty()).then(|| Multisig {
            threshold: u64::from(account.multisig.threshold),
            weights: account
                .multisig
                .weights
                .into_iter()
                .map(u64::from)
                .collect(),
            addresses: account
                .multisig
                .addresses
                .into_iter()
                .map(print_address)
                .collect(),
        }),
        ..Default::default()
    };
    let mut totals = BTreeMap::<u32, Amount>::new();
    for (id, value) in balances {
        budget.check()?;
        let asset = &assets[&id];
        result
            .balance
            .push(asset.balance(&value, &asset.bip_value(&value)?));
        totals.insert(id, value);
    }
    for (id, (value, delegate_bip_value)) in stakes {
        budget.check()?;
        let asset = &assets[&id];
        result.delegated.push(AddressDelegatedBalance {
            coin: Some(asset.reference()),
            value: value.to_string(),
            bip_value: asset.bip_value(&value)?.to_string(),
            delegate_bip_value: delegate_bip_value.to_string(),
        });
        *totals.entry(id).or_default() += value;
    }
    let mut bip = Amount::ZERO;
    for (id, value) in totals {
        budget.check()?;
        let asset = &assets[&id];
        let bip_value = asset.bip_value(&value)?;
        if delegated {
            result.total.push(asset.balance(&value, &bip_value));
        }
        bip += bip_value;
    }
    result.bip_value = bip.to_string();
    Ok(result)
}
