use crate::commission::Fees;
use crate::reward_price::Price;
use crate::stakes::genesis::{GenesisCandidate, GenesisDeletedCandidate};
use crate::state::{
    ABSENCE_WINDOW, Account, Amount, AppValues, Multisig, PublicKey, Validator, set,
    write_validators,
};
use crate::{coins, frozen, orders::Order, pool, stakes, updates};
use anyhow::{Context, Result, ensure};
use minter_protocol::{Address, state_keys::used_check_key};
use minter_state::{BlockStore, GenesisStage, Mutation, OperationControl};
use serde::{Deserialize, Deserializer, de::IgnoredAny};
use sha2::{Digest, Sha256};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
};

pub(crate) mod control;

#[derive(Deserialize)]
struct Genesis {
    #[serde(default, rename = "note")]
    _note: String,
    version: String,
    #[serde(default)]
    versions: Vec<serde_json::Value>,
    #[serde(default)]
    next_order_id: Integer,
    #[serde(default)]
    accounts: Vec<GenesisAccount>,
    #[serde(default)]
    validators: Vec<GenesisValidator>,
    candidates: Vec<GenesisCandidate>,
    #[serde(default, deserialize_with = "nullable_list")]
    coins: Vec<GenesisCoin>,
    #[serde(default, deserialize_with = "nullable_list")]
    block_list_candidates: Vec<String>,
    #[serde(default, deserialize_with = "nullable_list")]
    deleted_candidates: Vec<GenesisDeletedCandidate>,
    #[serde(default, deserialize_with = "nullable_list")]
    waitlist: Vec<GenesisWaitItem>,
    #[serde(default, deserialize_with = "nullable_list")]
    frozen_funds: Vec<GenesisFund>,
    #[serde(default, deserialize_with = "nullable_list")]
    pools: Vec<GenesisPool>,
    #[serde(default, deserialize_with = "nullable_list")]
    used_checks: Vec<String>,
    #[serde(default, deserialize_with = "nullable_list")]
    update_votes: Vec<GenesisUpdateVote>,
    #[serde(default, deserialize_with = "nullable_list")]
    commission_votes: Vec<GenesisCommissionVote>,
    // Go exports halt votes to JSON, but State.Import does not apply them.
    #[serde(default, rename = "halt_blocks")]
    _halt_blocks: Option<Vec<IgnoredAny>>,
    commission: BTreeMap<String, String>,
    #[serde(deserialize_with = "decimal")]
    emission: Amount,
    #[serde(deserialize_with = "decimal")]
    total_slashed: Amount,
    max_gas: Integer,
    prev_reward: Reward,
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct GenesisCommissionVote {
    height: Integer,
    #[serde(default, deserialize_with = "nullable_list")]
    votes: Vec<String>,
    commission: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct GenesisUpdateVote {
    height: Integer,
    version: String,
    #[serde(default, deserialize_with = "nullable_list")]
    votes: Vec<String>,
}

#[derive(Deserialize)]
struct GenesisAccount {
    #[serde(deserialize_with = "address")]
    address: Address,
    #[serde(default)]
    balance: Vec<Balance>,
    nonce: Integer,
    #[serde(default)]
    lock_stake_until_block: Integer,
    multisig_data: Option<GenesisMultisig>,
}

#[derive(Deserialize)]
struct GenesisMultisig {
    threshold: Integer,
    weights: Vec<Integer>,
    addresses: Vec<String>,
}

impl TryFrom<GenesisMultisig> for Multisig {
    type Error = anyhow::Error;

    fn try_from(input: GenesisMultisig) -> Result<Self> {
        ensure!(
            input.weights.len() == input.addresses.len(),
            "invalid genesis multisig weights"
        );
        Ok(Self {
            threshold: u32::try_from(input.threshold.0)?,
            weights: input
                .weights
                .into_iter()
                .map(|weight| u32::try_from(weight.0))
                .collect::<Result<Vec<_>, _>>()?,
            addresses: input
                .addresses
                .iter()
                .map(|address| hex_key(address, "Mx"))
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

#[derive(Deserialize)]
struct Balance {
    #[serde(default)]
    coin: Integer,
    #[serde(deserialize_with = "decimal")]
    value: Amount,
}

#[derive(Deserialize)]
struct GenesisValidator {
    public_key: String,
    #[serde(deserialize_with = "decimal")]
    total_bip_stake: Amount,
    #[serde(deserialize_with = "decimal")]
    accum_reward: Amount,
    absent_times: String,
}

#[derive(Deserialize)]
struct GenesisCoin {
    id: Integer,
    name: String,
    symbol: String,
    #[serde(default)]
    crr: Integer,
    #[serde(default)]
    version: Integer,
    owner_address: Option<String>,
    #[serde(default)]
    mintable: bool,
    #[serde(default)]
    burnable: bool,
    #[serde(deserialize_with = "decimal")]
    volume: Amount,
    #[serde(default)]
    reserve: Option<String>,
    #[serde(deserialize_with = "decimal")]
    max_supply: Amount,
}

#[derive(Deserialize)]
struct GenesisWaitItem {
    #[serde(deserialize_with = "address")]
    owner: Address,
    candidate_id: Integer,
    #[serde(default)]
    coin: Integer,
    #[serde(deserialize_with = "decimal")]
    value: Amount,
}

#[derive(Deserialize)]
struct GenesisFund {
    height: Integer,
    #[serde(deserialize_with = "address")]
    address: Address,
    candidate_key: Option<String>,
    #[serde(default)]
    candidate_id: Integer,
    #[serde(default)]
    move_to_candidate_id: Integer,
    #[serde(default)]
    coin: Integer,
    #[serde(deserialize_with = "decimal")]
    value: Amount,
}

#[derive(Deserialize)]
struct GenesisPool {
    id: Integer,
    #[serde(default)]
    coin0: Integer,
    #[serde(default)]
    coin1: Integer,
    #[serde(deserialize_with = "decimal")]
    reserve0: Amount,
    #[serde(deserialize_with = "decimal")]
    reserve1: Amount,
    #[serde(default)]
    orders: Vec<GenesisOrder>,
}

#[derive(Deserialize)]
struct GenesisOrder {
    id: Integer,
    is_sale: bool,
    #[serde(deserialize_with = "decimal")]
    volume0: Amount,
    #[serde(deserialize_with = "decimal")]
    volume1: Amount,
    #[serde(deserialize_with = "address")]
    owner: Address,
    height: Integer,
}

#[derive(Deserialize)]
struct Reward {
    time: Integer,
    #[serde(default, deserialize_with = "decimal")]
    amount_bip: Amount,
    #[serde(default, deserialize_with = "decimal")]
    amount_usdt: Amount,
    #[serde(default)]
    off: bool,
    #[serde(deserialize_with = "decimal")]
    reward: Amount,
}

#[derive(Default, Deserialize)]
pub(crate) struct Integer(#[serde(deserialize_with = "integer")] pub u64);

impl Integer {
    pub fn u32(self) -> Result<u32> {
        Ok(u32::try_from(self.0)?)
    }
}

fn integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Encoding {
        Text(String),
        Number(u64),
    }
    match Encoding::deserialize(deserializer)? {
        Encoding::Text(text) => text.parse().map_err(serde::de::Error::custom),
        Encoding::Number(value) => Ok(value),
    }
}

pub(crate) fn decimal<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Amount, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn nullable_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

fn hex_key<const N: usize>(text: &str, prefix: &str) -> Result<[u8; N]> {
    let mut key = [0; N];
    hex::decode_to_slice(
        text.strip_prefix(prefix)
            .context("invalid Minter key prefix")?,
        &mut key,
    )?;
    Ok(key)
}

pub(crate) fn address<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Address, D::Error> {
    hex_key(&String::deserialize(deserializer)?, "Mx").map_err(serde::de::Error::custom)
}

pub(crate) fn public_key<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<PublicKey, D::Error> {
    hex_key(&String::deserialize(deserializer)?, "Mp").map_err(serde::de::Error::custom)
}

#[derive(Default)]
pub(crate) struct Volumes(BTreeMap<u32, Amount>);

impl Volumes {
    pub fn take(&mut self, coin: u32, value: &Amount) -> Result<()> {
        if coin == 0 {
            return Ok(());
        }
        let volume = self
            .0
            .get_mut(&coin)
            .with_context(|| format!("unknown genesis coin {coin}"))?;
        ensure!(
            &*volume >= value,
            "genesis allocations exceed volume of coin {coin}"
        );
        *volume -= value;
        Ok(())
    }

    fn check(&self) -> Result<()> {
        for (coin, volume) in &self.0 {
            ensure!(
                volume.bits() == 0,
                "genesis allocations lack {volume} of coin {coin}"
            );
        }
        Ok(())
    }
}

pub(crate) struct InitialState {
    pub operations: Vec<Mutation>,
    pub validators: Vec<Validator>,
    pub fees: Fees,
    pub emission: Amount,
    pub reward_price: Price,
}

// Tendermint persists app_state as Go json.RawMessage. Marshaling it removes
// whitespace between tokens and escapes HTML characters inside strings.
pub(crate) fn hash(bytes: &[u8], control: &OperationControl) -> Result<[u8; 32]> {
    control.genesis_progress(GenesisStage::CanonicalHashBytes, 0)?;
    let mut hash = Sha256::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index.is_multiple_of(control::CHUNK_BYTES) {
            if index >= start {
                hash.update(&bytes[start..index]);
                start = index;
            }
            control.genesis_progress(GenesisStage::CanonicalHashBytes, index as u64)?;
        }
        if escaped {
            escaped = false;
            continue;
        }
        let (width, replacement): (usize, &[u8]) = match byte {
            b'"' => {
                quoted = !quoted;
                continue;
            }
            b'\\' if quoted => {
                escaped = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' if !quoted => (1, b""),
            b'<' if quoted => (1, b"\\u003c"),
            b'>' if quoted => (1, b"\\u003e"),
            b'&' if quoted => (1, b"\\u0026"),
            0xe2 if quoted && bytes.get(index + 1..index + 3) == Some(b"\x80\xa8") => {
                (3, b"\\u2028")
            }
            0xe2 if quoted && bytes.get(index + 1..index + 3) == Some(b"\x80\xa9") => {
                (3, b"\\u2029")
            }
            _ => continue,
        };
        hash.update(&bytes[start..index]);
        hash.update(replacement);
        start = index + width;
    }
    hash.update(&bytes[start..]);
    control.genesis_progress(GenesisStage::CanonicalHashBytes, bytes.len() as u64)?;
    Ok(hash.finalize().into())
}

pub(crate) fn matches_hash(
    bytes: &[u8],
    stored: [u8; 32],
    control: &OperationControl,
) -> Result<bool> {
    let raw = control::hash_bytes(bytes, GenesisStage::AppStateHashBytes, control)?;
    if stored == raw {
        // Existing databases can have the original byte hash.
        return Ok(true);
    }
    if stored != hash(bytes, control)? {
        return Ok(false);
    }
    // Whitespace may only be removed between valid JSON tokens.
    control::decode::<IgnoredAny>(bytes, GenesisStage::AppStateBytes, control)
        .context("invalid repeated genesis")?;
    Ok(true)
}

pub(crate) fn prepare(store: &BlockStore, network: u8, bytes: &[u8]) -> Result<InitialState> {
    let control = store.maintenance_control();
    let genesis: Genesis = control::decode(bytes, GenesisStage::AppStateBytes, control)
        .context("decode Go Minter genesis")?;
    ensure!(
        genesis.version == "v300" && genesis.versions.is_empty(),
        "unsupported genesis rules"
    );
    if let Some(name) = genesis.other.keys().next() {
        anyhow::bail!("unsupported genesis field {name}");
    }

    let coin_count = u32::try_from(genesis.coins.len())?;
    let (coins, mut volumes) = prepare_coins(genesis.coins)?;
    let mut operations = Vec::new();
    let mut accounts = genesis.accounts;
    accounts.sort_unstable_by_key(|row| Reverse(row.address));
    ensure!(
        accounts
            .windows(2)
            .all(|pair| pair[0].address != pair[1].address),
        "duplicate genesis account"
    );
    for row in accounts {
        let mut account = Account::new(row.address);
        account.set_stake_lock(row.lock_stake_until_block.0);
        if let Some(multisig) = row.multisig_data {
            account.multisig = multisig.try_into()?;
        }
        account.set_nonce(row.nonce.0);
        let mut seen = BTreeSet::new();
        for balance in row.balance {
            let coin = balance.coin.u32()?;
            ensure!(seen.insert(coin), "duplicate genesis balance");
            volumes.take(coin, &balance.value)?;
            account.set_balance(coin, balance.value);
        }
        account.write(&mut operations);
    }
    let app = AppValues {
        dirty: genesis.max_gas.0 != 0
            || genesis.total_slashed.bits() != 0
            || genesis.prev_reward.reward.bits() != 0
            || coin_count != 0,
        max_gas: genesis.max_gas.0,
        slashed: genesis.total_slashed,
        reward: genesis.prev_reward.reward.clone(),
        safe_reward: genesis.prev_reward.reward.clone(),
        coins: coin_count,
    };
    app.write(&mut operations);

    let blocked = genesis
        .block_list_candidates
        .iter()
        .map(|key| hex_key(key, "Mp"))
        .collect::<Result<_>>()?;
    let mut candidates = stakes::Candidates::from_genesis(
        genesis.candidates,
        blocked,
        genesis.deleted_candidates,
        &mut volumes,
    )?;
    let mut waitlist = stakes::Waitlist::default();
    candidates.recalculate(
        &coins::View {
            store,
            height: 0,
            network,
            changes: Some(&coins),
        },
        &mut waitlist,
        &mut Vec::new(),
    )?;
    coins.write(&mut operations);
    candidates.write(store, 0, &mut operations, &mut Vec::new())?;

    let mut original_validators = Vec::new();
    let mut seen = BTreeSet::new();
    for validator in genesis.validators {
        let key = hex_key(&validator.public_key, "Mp")?;
        ensure!(seen.insert(key), "duplicate genesis validator");
        let mut absent = 0;
        ensure!(
            validator.absent_times.len() == ABSENCE_WINDOW as usize,
            "invalid absence window in genesis"
        );
        for (index, bit) in validator.absent_times.bytes().enumerate() {
            match bit {
                b'x' => absent |= 1 << index,
                b'_' => (),
                _ => anyhow::bail!("invalid absence bit"),
            }
        }
        original_validators.push(Validator {
            key,
            absent,
            stake: validator.total_bip_stake,
            reward: validator.accum_reward,
            dirty: true,
            reward_dirty: true,
        });
    }
    write_validators(&original_validators, Vec::new(), &mut operations);
    let validators = candidates.validators(&original_validators);
    ensure!(!validators.is_empty(), "genesis has no eligible validators");

    let mut checks = BTreeSet::new();
    for hash in genesis.used_checks {
        let mut decoded = [0; 32];
        hex::decode_to_slice(hash, &mut decoded).context("invalid genesis check hash")?;
        checks.insert(decoded);
    }
    for hash in checks.into_iter().rev() {
        operations.push(set(used_check_key(hash), vec![1]));
    }

    let mut frozen = frozen::Changes::default();
    for row in genesis.frozen_funds {
        let id = row.candidate_id.u32()?;
        let destination = row.move_to_candidate_id.u32()?;
        let source = row
            .candidate_key
            .as_deref()
            .map(|key| hex_key(key, "Mp").map(|key| (key, id)))
            .transpose()?;
        ensure!(
            source.is_some() || (id == 0 && destination == 0),
            "genesis frozen stake lacks a candidate key"
        );
        let coin = row.coin.u32()?;
        volumes.take(coin, &row.value)?;
        frozen.push(
            store,
            0,
            row.height.0,
            frozen::Fund::new(row.address, coin, row.value, source, destination),
        )?;
    }
    frozen.write(&mut operations)?;

    for row in genesis.waitlist {
        let candidate = row.candidate_id.u32()?;
        ensure!(
            candidates.contains_genesis_id(candidate),
            "unknown waitlisted candidate {candidate}"
        );
        let coin = row.coin.u32()?;
        volumes.take(coin, &row.value)?;
        waitlist.import(row.owner, candidate, coin, row.value);
    }
    waitlist.write(&mut operations);
    write_pools(
        genesis.pools,
        genesis.next_order_id.u32()?,
        &mut volumes,
        &mut operations,
    )?;
    volumes.check()?;

    let fees = Fees::from_genesis(&genesis.commission)?;
    let commission_coin = fees.price_coin;
    ensure!(
        commission_coin == 0 || volumes.0.contains_key(&commission_coin),
        "unknown genesis commission coin {commission_coin}"
    );
    operations.push(set(b"p".to_vec(), fees.encode()));
    let mut commissions = updates::CommissionVotes::default();
    for row in genesis.commission_votes {
        let voters = row
            .votes
            .iter()
            .map(|voter| hex_key(voter, "Mp"))
            .collect::<Result<Vec<_>>>()?;
        commissions.import(
            row.height.0,
            &Fees::from_genesis(&row.commission)?.encode(),
            voters,
        );
    }
    commissions.write(&mut operations);
    let mut updates = updates::NetworkVotes::default();
    for row in genesis.update_votes {
        let voters = row
            .votes
            .iter()
            .map(|voter| hex_key(voter, "Mp"))
            .collect::<Result<Vec<_>>>()?;
        updates.import(row.height.0, row.version.as_bytes(), voters);
    }
    updates.write(&mut operations);

    Ok(InitialState {
        operations,
        validators,
        fees,
        emission: genesis.emission,
        reward_price: Price {
            time: genesis.prev_reward.time.0,
            bip: genesis.prev_reward.amount_bip,
            usdt: genesis.prev_reward.amount_usdt,
            off: genesis.prev_reward.off,
            last: genesis.prev_reward.reward,
        },
    })
}

fn prepare_coins(rows: Vec<GenesisCoin>) -> Result<(coins::Changes, Volumes)> {
    let mut coins = coins::Changes::default();
    let mut volumes = Volumes::default();
    for row in rows {
        let id = row.id.u32()?;
        ensure!(
            id != 0 && volumes.0.insert(id, row.volume.clone()).is_none(),
            "invalid or duplicate genesis coin ID"
        );
        let ratio = row.crr.u32()?;
        ensure!(ratio <= 100, "invalid genesis coin reserve ratio");
        let mut symbol = coins::Symbol([0; 10]);
        ensure!(
            !row.symbol.is_empty() && row.symbol.len() <= symbol.0.len(),
            "invalid genesis coin symbol"
        );
        symbol.0[..row.symbol.len()].copy_from_slice(row.symbol.as_bytes());
        let owner = row
            .owner_address
            .as_deref()
            .map(|address| hex_key(address, "Mx"))
            .transpose()?;
        coins.register(
            id,
            coins::Coin {
                name: row.name.into_bytes(),
                ratio,
                max_supply: row.max_supply,
                version: u16::try_from(row.version.0)?,
                symbol,
                mintable: ratio == 0 && row.mintable,
                burnable: ratio == 0 && row.burnable,
            },
            coins::Supply {
                volume: row.volume,
                reserve: if ratio == 0 {
                    Amount::ZERO
                } else {
                    row.reserve
                        .context("missing genesis coin reserve")?
                        .parse()
                        .context("invalid genesis coin reserve")?
                },
            },
            owner,
        );
    }
    Ok((coins, volumes))
}

fn write_pools(
    rows: Vec<GenesisPool>,
    next_order_id: u32,
    volumes: &mut Volumes,
    operations: &mut Vec<Mutation>,
) -> Result<()> {
    let next_pool_id = u32::try_from(rows.len() + 1)?;
    let mut pools = pool::Changes::default();
    let mut orders = Vec::new();
    let mut ids = BTreeSet::new();
    for row in rows {
        let id = row.id.u32()?;
        ensure!(ids.insert(id), "duplicate genesis pool ID");
        let pair = [row.coin0.u32()?, row.coin1.u32()?];
        volumes.take(pair[0], &row.reserve0)?;
        volumes.take(pair[1], &row.reserve1)?;
        pools.register_pool(pair, [row.reserve0, row.reserve1], id)?;
        for order in row.orders {
            if order.is_sale {
                volumes.take(pair[1], &order.volume1)?;
            } else {
                volumes.take(pair[0], &order.volume0)?;
            }
            orders.push(Order {
                id: order.id.u32()?,
                want_buy: order.volume0,
                want_sell: order.volume1,
                is_buy: !order.is_sale,
                pair,
                owner: order.owner,
                height: order.height.0,
            });
        }
    }
    pools.orders.import(next_order_id, orders)?;
    if next_pool_id > 1 {
        operations.push(set(
            pool::NEXT_POOL_ID.to_vec(),
            alloy_rlp::encode(next_pool_id),
        ));
    }
    pools.write(operations)
}

#[cfg(test)]
#[test]
fn genesis_hash_accepts_tendermint_reserialization() -> Result<()> {
    let original = "{\n \"note\": \" <\\\"&>\u{2028}\u{2029} \", \"height\": 10 \n}";
    let saved = br#"{"note":" \u003c\"\u0026\u003e\u2028\u2029 ","height":10}"#;
    let digest = Sha256::digest(saved).into();
    let control = OperationControl::default();
    assert_eq!(hash(original.as_bytes(), &control)?, digest);
    assert!(matches_hash(original.as_bytes(), digest, &control)?);
    assert!(matches_hash(saved, digest, &control)?);
    assert!(!matches_hash(
        br#"{"note":" <\"&>\u2028\u2029 ","height":11}"#,
        digest,
        &control
    )?);
    assert!(
        matches_hash(
            br#"{"note":" <\"&>\u2028\u2029 ","height":1 0}"#,
            digest,
            &control
        )
        .is_err()
    );
    Ok(())
}

#[cfg(test)]
#[test]
fn canonical_genesis_hash_handles_chunk_boundaries_and_cancelled_revalidation() -> Result<()> {
    let prefix = "{\n \"note\": \"";
    let padding = "a".repeat(control::CHUNK_BYTES - 1 - prefix.len());
    let original = format!("{prefix}{padding}\u{2028}<&>\", \"height\": 10 }}");
    let saved = format!(r#"{{"note":"{padding}\u2028\u003c\u0026\u003e","height":10}}"#);
    let expected: [u8; 32] = Sha256::digest(saved.as_bytes()).into();
    assert_eq!(
        hash(original.as_bytes(), &OperationControl::default())?,
        expected
    );
    for phase in [
        GenesisStage::AppStateHashBytes,
        GenesisStage::CanonicalHashBytes,
        GenesisStage::AppStateBytes,
    ] {
        let control = OperationControl::default();
        let stop = control.clone();
        let control = control.with_genesis_progress(move |current, done| {
            if current == phase && done >= control::CHUNK_BYTES as u64 {
                stop.cancel();
            }
        });
        let error = matches_hash(original.as_bytes(), expected, &control).unwrap_err();
        assert!(
            minter_state::OperationCancelled::caused(&error),
            "{phase}: {error:#}"
        );
    }
    Ok(())
}
