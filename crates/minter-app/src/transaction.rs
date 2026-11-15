mod bancor_trade;
mod coin;
mod create_pool;
mod gas;
#[cfg(test)]
mod history_tests;
mod liquidity;
mod multisig;
mod order;
mod redeem_check;
mod set_halt_block;
mod stake;
mod swap;
mod vote_commission;
mod vote_update;

use crate::{
    coins::View,
    commission::Fees,
    halts, pool, stakes,
    state::{Account, Amount, PublicKey, amount, display_key, finish, full_list},
    updates,
};
use alloy_rlp::{Decodable, Header};
use anyhow::{Context, Result, bail, ensure};
pub(crate) use bancor_trade::BancorChange;
use bancor_trade::{BancorAmount, BancorTrade};
use coin::{Creation, EditOwner, OwnerChange, Recreation, SupplyChange};
use create_pool::CreatePool;
pub(crate) use create_pool::{LOCKED_LIQUIDITY, PoolCreation};
pub(crate) use gas::GasPayment;
pub(crate) use liquidity::LiquidityChange;
use liquidity::{LiquidityKind, LiquidityTransaction};
use minter_protocol::{
    Address, Authorization, DecodeError, Envelope, Fields, InvalidSignature, MAX_TX_BYTES,
    Signature, SigningHash,
};
use multisig::MultisigTransaction;
use num_bigint::{BigInt, Sign};
pub(crate) use order::OrderAction;
use order::OrderTransaction;
use redeem_check::{RedeemCheck, Redemption};
pub(crate) use stake::StakeChange;
use stake::StakeTransaction;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use swap::{PoolTrade, SwapAmount, SwapRoute, minimum_output};
use tendermint_proto::v0_34::abci;
use vote_commission::VoteCommission;
use vote_update::VoteUpdate;

pub(crate) const DEFAULT_MIN_GAS_PRICE: u32 = 1;

pub(crate) fn total_gas(
    fields: Fields<'_>,
    commission_coin: u32,
    kind_gas: i64,
    multisig_signatures: usize,
) -> i64 {
    15 + i64::from(commission_coin != 0)
        + kind_gas
        + ((fields.payload.len() + fields.service_data.len()) / 1000) as i64
        + 20 * multisig_signatures as i64
}

#[derive(Default)]
pub(crate) struct Outcome {
    pub code: u32,
    log: String,
    info: String,
    gas: i64,
    tags: Vec<abci::EventAttribute>,
}

impl Outcome {
    pub fn reject<const N: usize>(code: u32, log: String, details: [(&str, String); N]) -> Self {
        let mut info = format!(r#"{{"code":"{code}""#);
        for (key, value) in details {
            let quoted = serde_json::to_string(&value)
                .expect("serialize string")
                .replace('&', "\\u0026")
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029");
            write!(info, ",\"{key}\":{quoted}").expect("write to string");
        }
        info.push('}');
        Self {
            code,
            log,
            info,
            ..Self::default()
        }
    }

    pub fn check(self) -> abci::ResponseCheckTx {
        abci::ResponseCheckTx {
            code: self.code,
            log: self.log,
            info: self.info,
            gas_wanted: self.gas,
            gas_used: self.gas,
            events: vec![abci::Event {
                r#type: "tags".into(),
                attributes: Vec::new(),
            }],
            ..Default::default()
        }
    }

    pub fn deliver(self) -> abci::ResponseDeliverTx {
        abci::ResponseDeliverTx {
            code: self.code,
            log: self.log,
            info: self.info,
            gas_wanted: self.gas,
            gas_used: self.gas,
            events: vec![abci::Event {
                r#type: "tags".into(),
                attributes: self.tags,
            }],
            ..Default::default()
        }
    }
}

pub(crate) enum Prepared<'a> {
    Transaction(Transaction<'a>),
    Rejected(Outcome),
}

pub(crate) enum CommissionQuote {
    Price(Amount),
    Rejected(Outcome),
}

pub(crate) enum Evaluated {
    Accepted {
        gas: GasPayment,
        operation: Operation,
    },
    Rejected(Outcome),
}

impl Evaluated {
    pub fn account_to_check(&self) -> Option<Address> {
        match self {
            Self::Accepted {
                operation: Operation::Multisig(change),
                ..
            } => change.account_to_check(),
            Self::Accepted {
                operation: Operation::RedeemCheck(check),
                ..
            } => Some(check.issuer),
            _ => None,
        }
    }
}

pub(crate) enum Operation {
    Regular,
    Swap(SwapRoute),
    LimitOrder(OrderAction),
    Liquidity(Box<LiquidityChange>),
    CreatePool(Box<PoolCreation>),
    Bancor(BancorChange),
    Stake(StakeChange),
    Multisig(multisig::Change),
    RedeemCheck(Redemption),
    EditCoinOwner(OwnerChange),
    Recreate(Box<Recreation>),
    SetHaltBlock(halts::Vote),
    VoteCommission(updates::Vote),
    VoteUpdate(updates::Vote),
}

pub(crate) struct TransactionState<'a> {
    pub stakes: stakes::View<'a>,
    pub halts: Option<&'a halts::Votes>,
    pub commissions: Option<&'a updates::CommissionVotes>,
    pub updates: Option<&'a updates::NetworkVotes>,
    pub used_checks: Option<&'a BTreeSet<SigningHash>>,
}

impl TransactionState<'_> {
    fn owner_rejection(&self, key: PublicKey, sender: Address) -> Result<Option<Outcome>> {
        let Some(owner) = self.stakes.owner(key)? else {
            let public_key = display_key(key);
            return Ok(Some(Outcome::reject(
                403,
                format!("Candidate with such public key ({public_key}) not found"),
                [("public_key", public_key)],
            )));
        };
        Ok((owner != sender).then(|| {
            Outcome::reject(
                406,
                "Sender is not an owner of a candidate".into(),
                [
                    ("sender", display_address(sender)),
                    ("public_key", display_key(key)),
                    ("owner", display_address(owner)),
                ],
            )
        }))
    }
}

pub(crate) struct Transaction<'a> {
    pub sender: Address,
    pub nonce: u64,
    pub gas_coin: u32,
    gas_price: u32,
    price: BigInt,
    pub failed_fee: Amount,
    pub ticker_price: Option<Amount>,
    data: Data,
    multisig: Option<MultisigWitness<'a>>,
    gas: i64,
}

enum Data {
    Send(Payment),
    Multisend(Vec<Payment>),
    CreateCoin(Box<Creation>),
    CreateToken(Box<Creation>),
    RecreateCoin(Box<Creation>),
    RecreateToken(Box<Creation>),
    Supply(SupplyChange),
    EditCoinOwner(EditOwner),
    PoolTrade(PoolTrade),
    LimitOrder(OrderTransaction),
    Liquidity(LiquidityTransaction),
    CreatePool(CreatePool),
    Bancor(BancorTrade),
    Stake(StakeTransaction),
    Multisig(MultisigTransaction),
    RedeemCheck(RedeemCheck),
    SetHaltBlock(halts::Vote),
    VoteCommission(Box<VoteCommission>),
    VoteUpdate(VoteUpdate),
}

pub(crate) struct Payment {
    pub coin: u32,
    pub recipient: Address,
    pub value: Amount,
}

struct MultisigWitness<'a> {
    signing_hash: SigningHash,
    signatures: Vec<Signature<'a>>,
}

impl<'a> Transaction<'a> {
    pub fn prepare(
        raw: &'a [u8],
        fees: &Fees,
        view: &View<'_>,
        min_gas_price: Option<u32>,
    ) -> Result<Prepared<'a>> {
        let tx = match Envelope::decode(raw) {
            Ok(tx) => tx,
            Err(DecodeError::TooLarge) => {
                return Ok(Prepared::Rejected(Outcome::reject(
                    105,
                    format!("TX length is over {MAX_TX_BYTES} bytes"),
                    [
                        ("max_tx_length", MAX_TX_BYTES.to_string()),
                        ("got_tx_length", raw.len().to_string()),
                    ],
                )));
            }
            Err(error) => {
                let log = match error {
                    DecodeError::Rlp(_) if raw.is_empty() => "EOF".into(),
                    DecodeError::Rlp(alloy_rlp::Error::UnexpectedString)
                        if raw.first().is_some_and(|tag| *tag < 0xc0) =>
                    {
                        "rlp: expected input list for transaction.Transaction".into()
                    }
                    DecodeError::UnknownSignatureType(_) => "unknown signature type".into(),
                    _ => error.to_string(),
                };
                return Ok(Prepared::Rejected(Outcome::reject(106, log, [])));
            }
        };
        let fields = tx.fields();
        if !matches!(fields.kind, 0x01..=0x12 | 0x14..=0x26) {
            return unsupported(
                min_gas_price.is_some(),
                format!("transaction type {:02x} is unavailable", fields.kind),
            );
        }
        let data = match Data::decode(fields.kind, fields.data) {
            Ok(data) => data,
            Err(error) => {
                return Ok(Prepared::Rejected(Outcome::reject(
                    106,
                    error.to_string(),
                    [],
                )));
            }
        };
        if fields.kind == 0x25 && view.height < 10_197_360 {
            return Ok(Prepared::Rejected(Outcome::reject(
                124,
                "LockStake available from block 10197360 ".into(),
                [],
            )));
        }
        if fields.chain_id != view.network {
            return Ok(Prepared::Rejected(Outcome::reject(
                115,
                "Wrong chain id".into(),
                [
                    ("current_chain_id", view.network.to_string()),
                    ("got_chain_id", fields.chain_id.to_string()),
                ],
            )));
        }
        let gas_coin = match &data {
            Data::PoolTrade(data) if matches!(data.amount, SwapAmount::All) => {
                data.coins.first().copied().unwrap_or_default()
            }
            Data::Bancor(data) if matches!(data.kind, BancorAmount::SellAll) => data.from,
            _ => fields.gas_coin,
        };
        if gas_coin != 0 && view.coin(gas_coin)?.is_none() {
            return Ok(Prepared::Rejected(coin_not_found(gas_coin)));
        }
        if let Some(min_gas_price) = min_gas_price
            && fields.gas_price < min_gas_price
        {
            return Ok(Prepared::Rejected(Outcome::reject(
                114,
                format!(
                    "Gas price of tx is too low to be included in mempool. Expected {min_gas_price}"
                ),
                [
                    ("min_gas_price", min_gas_price.to_string()),
                    ("got_gas_price", fields.gas_price.to_string()),
                ],
            )));
        }
        if fields.payload.len() > 10000 {
            return Ok(Prepared::Rejected(Outcome::reject(
                109,
                "TX payload length is over 10000 bytes".into(),
                [
                    ("max_payload_length", "10000".into()),
                    ("got_payload_length", fields.payload.len().to_string()),
                ],
            )));
        }
        if fields.service_data.len() > 128 {
            return Ok(Prepared::Rejected(Outcome::reject(
                110,
                "TX service data length is over 128 bytes".into(),
                [
                    ("max_service_data_length", "128".into()),
                    (
                        "got_service_data_length",
                        fields.service_data.len().to_string(),
                    ),
                ],
            )));
        }
        let signing_hash = *tx.signing_hash();
        let (sender, multisig) = match tx.into_authorization() {
            Authorization::Single(signature) => match signature.recover(&signing_hash) {
                Ok(sender) => (sender, None),
                Err(error) => {
                    return Ok(Prepared::Rejected(Outcome::reject(
                        106,
                        signature_error(error).into(),
                        [],
                    )));
                }
            },
            Authorization::Multisig {
                account,
                signatures,
            } => (
                account,
                Some(MultisigWitness {
                    signing_hash,
                    signatures,
                }),
            ),
        };
        let payload_len = fields.payload.len() + fields.service_data.len();
        let payload_cost = &fees.payload_byte * payload_len;
        let ticker_price = data.creation().map(|creation| {
            fees.ticker_price(creation.coin.symbol.bytes().len()) * fields.gas_price
        });
        let mut price =
            (data.commission(fees) + BigInt::from(payload_cost.clone())) * fields.gas_price;
        if let Some(ticker_price) = &ticker_price {
            price += BigInt::from(ticker_price.clone());
        }
        let signatures = multisig
            .as_ref()
            .map_or(0, |witness| witness.signatures.len());
        let gas = total_gas(fields, gas_coin, data.gas(), signatures);
        Ok(Prepared::Transaction(Self {
            sender,
            nonce: fields.nonce,
            gas_coin,
            gas_price: fields.gas_price,
            price,
            failed_fee: (&fees.failed_tx + payload_cost) * fields.gas_price,
            ticker_price,
            data,
            multisig,
            gas,
        }))
    }

    pub fn payments(&self) -> &[Payment] {
        self.data.payments()
    }

    pub fn creation(&self) -> Option<&Creation> {
        self.data.creation()
    }

    pub fn recreation(&self) -> Option<&Creation> {
        self.data.recreation()
    }

    pub fn supply_change(&self) -> Option<&SupplyChange> {
        if let Data::Supply(change) = &self.data {
            Some(change)
        } else {
            None
        }
    }

    pub fn early_rejection(&self, sender: &Account, price_coin: u32) -> Option<Outcome> {
        if let Some(witness) = &self.multisig
            && let Some(rejected) = witness.rejection(sender, self.sender)
        {
            return Some(rejected);
        }
        let expected = sender.nonce.wrapping_add(1);
        if self.nonce != expected {
            return Some(Outcome::reject(
                101,
                format!(
                    "Unexpected nonce. Expected: {expected}, got {}.",
                    self.nonce
                ),
                [
                    ("expected_nonce", expected.to_string()),
                    ("got_nonce", self.nonce.to_string()),
                ],
            ));
        }
        (price_coin == 0 && self.price.sign() == Sign::Minus).then(unpayable_commission)
    }

    pub fn quote(
        &self,
        view: &View<'_>,
        pools: Option<&pool::Changes>,
        price_coin: u32,
    ) -> Result<CommissionQuote> {
        ensure!(
            self.price.sign() != Sign::Minus,
            "negative commission in a custom coin"
        );
        if self.price.sign() == Sign::NoSign {
            return Ok(CommissionQuote::Price(Amount::default()));
        }
        commission_quote(view, pools, price_coin, self.price.magnitude())
    }

    pub fn evaluate(
        &self,
        view: &View<'_>,
        pools: Option<&pool::Changes>,
        state: &TransactionState<'_>,
        sender: &mut Account,
        price_in_bip: Amount,
        check: bool,
    ) -> Result<Evaluated> {
        let pools = pool::View {
            store: view.store,
            height: view.height,
            changes: pools,
        };
        if let Some(outcome) = self.check_data(view, &pools)? {
            return Ok(Evaluated::Rejected(self.with_gas(outcome)));
        }
        let planned = match &self.data {
            Data::Stake(data) => data
                .plan(view, state, sender, self.sender)?
                .map(Operation::Stake),
            Data::Multisig(data) => data
                .plan(sender, self.sender, self.nonce)
                .map(Operation::Multisig),
            Data::RedeemCheck(data) => data.plan(view, state, self)?.map(Operation::RedeemCheck),
            Data::EditCoinOwner(data) => {
                data.plan(view, self.sender)?.map(Operation::EditCoinOwner)
            }
            Data::RecreateCoin(data) | Data::RecreateToken(data) => data
                .plan_recreation(
                    view,
                    self.sender,
                    matches!(&self.data, Data::RecreateToken(_)),
                )?
                .map(|change| Operation::Recreate(Box::new(change))),
            Data::SetHaltBlock(data) => data.plan(state, self.sender)?,
            Data::VoteCommission(data) => data
                .plan(view, &pools, state, self.sender)?
                .map(Operation::VoteCommission),
            Data::VoteUpdate(data) => data.plan(state, self.sender)?.map(Operation::VoteUpdate),
            _ => Ok(Operation::Regular),
        };
        let planned = match planned {
            Ok(operation) => operation,
            Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
        };
        let gas = match GasPayment::choose(view, &pools, self.gas_coin, price_in_bip)? {
            Ok(gas) => gas,
            Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
        };
        let operation = match &self.data {
            Data::PoolTrade(data) => {
                match data.plan(view, &pools, self.sender, sender, &gas, check)? {
                    Ok(route) => Operation::Swap(route),
                    Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
                }
            }
            Data::LimitOrder(data) => match data.plan(view, &pools, self.sender, sender, &gas)? {
                Ok(order) => Operation::LimitOrder(order),
                Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
            },
            Data::Liquidity(data) => match data.plan(view, &pools, self, sender, &gas, check)? {
                Ok(change) => Operation::Liquidity(Box::new(change)),
                Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
            },
            Data::CreatePool(data) => match data.plan(view, &pools, self, sender, &gas)? {
                Ok(change) => Operation::CreatePool(Box::new(change)),
                Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
            },
            Data::Bancor(data) => match data.plan(view, self, sender, &gas)? {
                Ok(change) => Operation::Bancor(change),
                Err(outcome) => return Ok(Evaluated::Rejected(self.with_gas(outcome))),
            },
            Data::RedeemCheck(_) => planned,
            _ => {
                if let Some(outcome) = self.check_balances(view, sender, &gas)? {
                    return Ok(Evaluated::Rejected(self.with_gas(outcome)));
                }
                if matches!(
                    &planned,
                    Operation::Stake(StakeChange::CandidateStatus {
                        online: true,
                        jailed: true,
                        ..
                    })
                ) {
                    return Ok(Evaluated::Rejected(self.with_gas(Outcome::reject(
                        414,
                        "Candidate is jailed".into(),
                        [],
                    ))));
                }
                if let Operation::Stake(StakeChange::EditPublicKey {
                    new, blocked: true, ..
                }) = &planned
                {
                    let key = display_key(*new);
                    return Ok(Evaluated::Rejected(self.with_gas(Outcome::reject(
                        410,
                        format!("Public key ({key}) exists in block list"),
                        [("public_key", key)],
                    ))));
                }
                planned
            }
        };
        Ok(Evaluated::Accepted { gas, operation })
    }

    pub fn check_related_account(
        &self,
        evaluated: Evaluated,
        view: &View<'_>,
        account: &mut Account,
    ) -> Result<Evaluated> {
        let Evaluated::Accepted { gas, operation } = &evaluated else {
            return Ok(evaluated);
        };
        let rejection = match operation {
            Operation::Multisig(change) => change.rejection(account),
            Operation::RedeemCheck(check) => {
                check.balance_rejection(view, account, self.sender, gas)?
            }
            _ => None,
        };
        Ok(match rejection {
            Some(outcome) => Evaluated::Rejected(self.with_gas(outcome)),
            None => evaluated,
        })
    }

    fn check_data(&self, view: &View<'_>, pools: &pool::View<'_>) -> Result<Option<Outcome>> {
        if let Some(creation) = self.creation() {
            return creation.rejection(view, matches!(&self.data, Data::CreateToken(_)));
        }
        if let Some(change) = self.supply_change() {
            return change.rejection(view, self.sender);
        }
        if let Data::PoolTrade(data) = &self.data {
            return data.check_path(pools);
        }
        if let Data::LimitOrder(data) = &self.data {
            return data.check_data(pools);
        }
        if let Data::Liquidity(data) = &self.data {
            return data.check_data(view, pools);
        }
        if let Data::CreatePool(data) = &self.data {
            return data.rejection(view, pools);
        }
        if let Data::Bancor(data) = &self.data {
            return data.check_data(view);
        }
        let payments = self.payments();
        if matches!(&self.data, Data::Multisend(_)) && !(1..=100).contains(&payments.len()) {
            return Ok(Some(Outcome::reject(
                111,
                "List length must be between 1 and 100".into(),
                [
                    ("min_quantity", "1".into()),
                    ("max_quantity", "100".into()),
                    ("got_quantity", payments.len().to_string()),
                ],
            )));
        }
        for payment in payments {
            if view.symbol(payment.coin)?.is_none() {
                return Ok(Some(coin_not_found(payment.coin)));
            }
        }
        Ok(None)
    }

    fn check_balances(
        &self,
        view: &View<'_>,
        sender: &mut Account,
        gas: &GasPayment,
    ) -> Result<Option<Outcome>> {
        let gas_coin = gas.coin();
        if let Data::Stake(StakeTransaction::Declare(data)) = &self.data {
            for (coin, amount) in [(data.coin, &data.value), (gas_coin, gas.amount())] {
                if let Some(outcome) = self.balance_rejection(view, sender, coin, amount)? {
                    return Ok(Some(outcome));
                }
            }
            if data.coin == gas_coin {
                return self.balance_rejection(
                    view,
                    sender,
                    gas_coin,
                    &(&data.value + gas.amount()),
                );
            }
            return Ok(None);
        }
        if let Data::Stake(StakeTransaction::Delegate(data)) = &self.data {
            if let Some(outcome) = self.balance_rejection(view, sender, gas_coin, gas.amount())? {
                return Ok(Some(outcome));
            }
            if let Some(outcome) = self.balance_rejection(view, sender, data.coin, &data.value)? {
                return Ok(Some(outcome));
            }
            if data.coin == gas_coin {
                return self.balance_rejection(
                    view,
                    sender,
                    gas_coin,
                    &(&data.value + gas.amount()),
                );
            }
            return Ok(None);
        }
        if let Data::Stake(StakeTransaction::CandidateStatus { online: false, .. }) = &self.data {
            let mut outcome = self.balance_rejection(view, sender, gas_coin, gas.amount())?;
            if let Some(rejection) = &mut outcome {
                rejection.log = format!(
                    "Insufficient funds for sender account: {}. Wanted {} {gas_coin}",
                    display_address(self.sender),
                    gas.amount()
                );
            }
            return Ok(outcome);
        }
        if let Data::Stake(StakeTransaction::Lock { coin, value, .. }) = &self.data {
            if *coin == gas_coin {
                return self.balance_rejection(view, sender, gas_coin, &(value + gas.amount()));
            }
            if let Some(outcome) = self.balance_rejection(view, sender, *coin, value)? {
                return Ok(Some(outcome));
            }
            return self.balance_rejection(view, sender, gas_coin, gas.amount());
        }
        if let Some(creation) = self.recreation() {
            if let Some(outcome) = self.balance_rejection(view, sender, gas_coin, gas.amount())? {
                return Ok(Some(outcome));
            }
            if let Some(outcome) =
                self.balance_rejection(view, sender, 0, &creation.supply.reserve)?
            {
                return Ok(Some(outcome));
            }
            return if gas_coin == 0 {
                self.balance_rejection(view, sender, 0, &(gas.amount() + &creation.supply.reserve))
            } else {
                Ok(None)
            };
        }
        if let Some(creation) = self.creation() {
            if let Some(rejection) = self.balance_rejection(view, sender, gas_coin, gas.amount())? {
                return Ok(Some(rejection));
            }
            let needed = if gas_coin == 0 {
                gas.amount() + &creation.supply.reserve
            } else {
                creation.supply.reserve.clone()
            };
            return self.balance_rejection(view, sender, 0, &needed);
        }
        if let Some(change) = self.supply_change() {
            let rejection = self.balance_rejection(view, sender, gas_coin, gas.amount())?;
            if rejection.is_some() || change.mint {
                return Ok(rejection);
            }
            let needed = if gas_coin == change.id {
                &change.value + gas.amount()
            } else {
                change.value.clone()
            };
            return self.balance_rejection(view, sender, change.id, &needed);
        }
        if let Data::Send(payment) = &self.data {
            if gas_coin == payment.coin {
                return self.balance_rejection(
                    view,
                    sender,
                    gas_coin,
                    &(&payment.value + gas.amount()),
                );
            }
            if let Some(rejection) =
                self.balance_rejection(view, sender, payment.coin, &payment.value)?
            {
                return Ok(Some(rejection));
            }
            return self.balance_rejection(view, sender, gas_coin, gas.amount());
        }
        let mut totals = BTreeMap::from([(gas_coin, gas.amount().clone())]);
        for payment in self.payments() {
            *totals.entry(payment.coin).or_default() += &payment.value;
        }
        // The executor reports the highest insufficient coin ID first.
        for (coin, needed) in totals.into_iter().rev() {
            if let Some(rejection) = self.balance_rejection(view, sender, coin, &needed)? {
                return Ok(Some(rejection));
            }
        }
        Ok(None)
    }

    fn balance_rejection(
        &self,
        view: &View<'_>,
        sender: &mut Account,
        coin: u32,
        needed: &Amount,
    ) -> Result<Option<Outcome>> {
        if sender.balance(view.store, view.height, coin)? < needed {
            let symbol = view.symbol(coin)?.context("missing balance coin")?;
            return Ok(Some(insufficient(self.sender, coin, &symbol, needed)));
        }
        Ok(None)
    }

    pub fn success(&self) -> Outcome {
        self.with_gas(Outcome::default())
    }

    pub fn failed_payer(&self, outcome: &mut Outcome) -> Result<Address, Outcome> {
        let Data::RedeemCheck(data) = &self.data else {
            return Ok(self.sender);
        };
        let issuer = data.issuer()?;
        outcome
            .tags
            .push(tag("tx.from", &hex::encode(issuer), false));
        Ok(issuer)
    }

    fn with_gas(&self, mut outcome: Outcome) -> Outcome {
        outcome.gas = self.gas;
        outcome
    }

    pub fn delivered(
        &self,
        outcome: &mut Outcome,
        price_coin: u32,
        gas: Option<&GasPayment>,
        creation: Option<(u32, &Amount)>,
        operation: Option<&Operation>,
    ) -> Result<()> {
        if outcome.code == 0 {
            let details = gas
                .context("successful transaction has no gas payment")?
                .tag_commission(&mut outcome.tags)?;
            if let Some(Operation::Liquidity(change)) = operation {
                change.tag_success(&mut outcome.tags, details);
            } else if let Some((id, ticker_burn)) = creation {
                outcome.tags.push(details);
                let symbol = self.creation().expect("created coin").coin.symbol;
                outcome.tags.extend([
                    tag("tx.coin_symbol", &symbol.name(), true),
                    tag("tx.coin_id", &id.to_string(), true),
                    tag("tx.burned_for_symbol", &ticker_burn.to_string(), false),
                ]);
            } else {
                outcome.tags.push(details);
                match operation {
                    Some(Operation::Swap(swap)) => swap.tag_success(&mut outcome.tags)?,
                    Some(Operation::LimitOrder(order)) => order.tag_success(&mut outcome.tags),
                    Some(Operation::Bancor(change)) => change.tag_success(&mut outcome.tags),
                    Some(Operation::Stake(change)) => change.tag_success(&mut outcome.tags),
                    Some(Operation::Multisig(change)) => change.tag_success(&mut outcome.tags),
                    Some(Operation::RedeemCheck(check)) => {
                        check.tag_success(self.sender, &mut outcome.tags)
                    }
                    Some(Operation::EditCoinOwner(change)) => change.tag_success(&mut outcome.tags),
                    Some(Operation::Recreate(change)) => change.tag_success(&mut outcome.tags),
                    Some(Operation::CreatePool(change)) => change.tag_success(&mut outcome.tags),
                    _ => self.data.tag_success(&mut outcome.tags),
                }
            }
        } else if let Some(fee) = gas {
            fee.tag_failure(&mut outcome.tags)?;
        }
        outcome.tags.extend([
            tag("tx.commission_price_coin", &price_coin.to_string(), false),
            tag("tx.commission_price", &self.price.to_string(), false),
            tag("tx.type", self.data.type_tag(), true),
            tag("tx.commission_coin", &self.gas_coin.to_string(), true),
        ]);
        if !matches!(self.data, Data::RedeemCheck(_)) {
            outcome
                .tags
                .push(tag("tx.from", &hex::encode(self.sender), true));
        }
        Ok(())
    }
}

impl Data {
    fn decode(kind: u8, raw: &[u8]) -> Result<Self> {
        let mut fields = full_list(raw)?;
        match kind {
            0x01 => Ok(Self::Send(Payment::decode(fields)?)),
            0x02..=0x04 => Ok(Self::Bancor(BancorTrade::decode(kind, fields)?)),
            0x05 => Ok(Self::CreateCoin(Box::new(Creation::decode(fields, false)?))),
            0x06..=0x08 | 0x0a | 0x0b | 0x0e | 0x14 | 0x1a | 0x1b | 0x25 | 0x26 => {
                Ok(Self::Stake(StakeTransaction::decode(kind, fields)?))
            }
            0x09 => Ok(Self::RedeemCheck(RedeemCheck::decode(fields)?)),
            0x0c | 0x12 => Ok(Self::Multisig(MultisigTransaction::decode(
                kind == 0x0c,
                fields,
            )?)),
            0x0f => Ok(Self::SetHaltBlock(halts::Vote::decode(fields)?)),
            0x10 => Ok(Self::RecreateCoin(Box::new(Creation::decode(
                fields, false,
            )?))),
            0x11 => Ok(Self::EditCoinOwner(EditOwner::decode(fields)?)),
            0x15 | 0x16 => Ok(Self::Liquidity(LiquidityTransaction::decode(
                kind == 0x15,
                fields,
            )?)),
            0x17..=0x19 => Ok(Self::PoolTrade(PoolTrade::decode(kind, fields)?)),
            0x23 | 0x24 => Ok(Self::LimitOrder(OrderTransaction::decode(
                kind == 0x23,
                fields,
            )?)),
            0x1e => Ok(Self::CreateToken(Box::new(Creation::decode(fields, true)?))),
            0x1f => Ok(Self::RecreateToken(Box::new(Creation::decode(
                fields, true,
            )?))),
            0x1c | 0x1d => Ok(Self::Supply(SupplyChange::decode(fields, kind == 0x1c)?)),
            0x20 => Ok(Self::VoteCommission(Box::new(VoteCommission::decode(
                fields,
            )?))),
            0x21 => Ok(Self::VoteUpdate(VoteUpdate::decode(fields)?)),
            0x22 => Ok(Self::CreatePool(CreatePool::decode(fields)?)),
            0x0d => {
                let mut items = Header::decode_bytes(&mut fields, true)?;
                finish(fields)?;
                let mut payments = Vec::new();
                while !items.is_empty() {
                    let item = Header::decode_bytes(&mut items, true)?;
                    payments.push(Payment::decode(item)?);
                }
                Ok(Self::Multisend(payments))
            }
            _ => bail!("unsupported transaction type"),
        }
    }

    fn payments(&self) -> &[Payment] {
        match self {
            Self::Send(payment) => std::slice::from_ref(payment),
            Self::Multisend(payments) => payments,
            _ => &[],
        }
    }

    fn creation(&self) -> Option<&Creation> {
        match self {
            Self::CreateCoin(creation) | Self::CreateToken(creation) => Some(creation),
            _ => None,
        }
    }

    fn recreation(&self) -> Option<&Creation> {
        match self {
            Self::RecreateCoin(creation) | Self::RecreateToken(creation) => Some(creation),
            _ => None,
        }
    }

    fn commission(&self, fees: &Fees) -> BigInt {
        match self {
            Self::Send(_) => BigInt::from(fees.send.clone()),
            Self::Multisend(payments) => {
                BigInt::from(fees.multisend_base.clone())
                    + BigInt::from(fees.multisend_delta.clone()) * (payments.len() as i64 - 1)
            }
            Self::CreateCoin(_) | Self::CreateToken(_) => BigInt::from(fees.create_coin.clone()),
            Self::RecreateCoin(_) => BigInt::from(fees.recreate_coin.clone()),
            Self::RecreateToken(_) => BigInt::from(fees.recreate_token.clone()),
            Self::Supply(change) => BigInt::from(if change.mint {
                fees.mint_token.clone()
            } else {
                fees.burn_token.clone()
            }),
            Self::PoolTrade(data) => {
                let (base, delta) = match data.amount {
                    SwapAmount::Sell(_) => (&fees.sell_pool_base, &fees.sell_pool_delta),
                    SwapAmount::Buy(_) => (&fees.buy_pool_base, &fees.buy_pool_delta),
                    SwapAmount::All => (&fees.sell_all_pool_base, &fees.sell_all_pool_delta),
                };
                BigInt::from(base.clone())
                    + BigInt::from(delta.clone()) * (data.coins.len() as i64 - 2)
            }
            Self::LimitOrder(data) => BigInt::from(match data {
                OrderTransaction::Place(_) => fees.add_limit_order.clone(),
                OrderTransaction::Cancel(_) => fees.remove_limit_order.clone(),
            }),
            Self::Liquidity(data) => BigInt::from(match data.kind {
                LiquidityKind::Add { .. } => fees.add_liquidity.clone(),
                LiquidityKind::Remove { .. } => fees.remove_liquidity.clone(),
            }),
            Self::SetHaltBlock(_) => BigInt::from(fees.set_halt_block.clone()),
            Self::RedeemCheck(_) => BigInt::from(fees.redeem_check.clone()),
            Self::EditCoinOwner(_) => BigInt::from(fees.edit_ticker_owner.clone()),
            Self::CreatePool(_) => BigInt::from(fees.create_swap_pool.clone()),
            Self::Multisig(data) => BigInt::from(if data.create {
                fees.create_multisig.clone()
            } else {
                fees.edit_multisig.clone()
            }),
            Self::VoteCommission(_) => BigInt::from(fees.vote_commission.clone()),
            Self::VoteUpdate(_) => BigInt::from(fees.vote_update.clone()),
            Self::Bancor(data) => BigInt::from(match data.kind {
                BancorAmount::Sell(_) => fees.sell_bancor.clone(),
                BancorAmount::SellAll => fees.sell_all_bancor.clone(),
                BancorAmount::Buy(_) => fees.buy_bancor.clone(),
            }),
            Self::Stake(data) => BigInt::from(match data {
                StakeTransaction::Declare(_) => fees.declare_candidacy.clone(),
                StakeTransaction::Delegate(_) => fees.delegate.clone(),
                StakeTransaction::Unbond(_) => fees.unbond.clone(),
                StakeTransaction::Move(_) => fees.move_stake.clone(),
                StakeTransaction::Edit(_) => fees.edit_candidate.clone(),
                StakeTransaction::EditPublicKey { .. } => fees.edit_candidate_public_key.clone(),
                StakeTransaction::EditCommission { .. } => fees.edit_candidate_commission.clone(),
                StakeTransaction::CandidateStatus { online: true, .. } => {
                    fees.set_candidate_on.clone()
                }
                StakeTransaction::CandidateStatus { online: false, .. } => {
                    fees.set_candidate_off.clone()
                }
                StakeTransaction::LockStake => fees.lock_stake.clone(),
                StakeTransaction::Lock { .. } => fees.lock.clone(),
            }),
        }
    }

    fn gas(&self) -> i64 {
        match self {
            Self::Send(_) | Self::Supply(_) => 1,
            Self::Multisend(payments) => 1 + payments.len() as i64,
            Self::CreateCoin(_) | Self::CreateToken(_) => 3,
            Self::RecreateCoin(_) | Self::RecreateToken(_) => 5,
            Self::PoolTrade(data) => data.coins.len() as i64,
            Self::LimitOrder(_) => 50,
            Self::RedeemCheck(_) => 20,
            Self::CreatePool(_) => 10,
            Self::Multisig(data) => {
                if data.create {
                    20
                } else {
                    5
                }
            }
            Self::Liquidity(_)
            | Self::SetHaltBlock(_)
            | Self::VoteCommission(_)
            | Self::VoteUpdate(_)
            | Self::EditCoinOwner(_)
            | Self::Stake(StakeTransaction::Edit(_)) => 5,
            Self::Bancor(_) => 2,
            Self::Stake(StakeTransaction::Declare(_) | StakeTransaction::EditPublicKey { .. }) => {
                10
            }
            Self::Stake(
                StakeTransaction::Delegate(_)
                | StakeTransaction::Unbond(_)
                | StakeTransaction::Move(_),
            ) => 6,
            Self::Stake(
                StakeTransaction::CandidateStatus { .. } | StakeTransaction::EditCommission { .. },
            ) => 1,
            Self::Stake(_) => 2,
        }
    }

    fn tag_success(&self, tags: &mut Vec<abci::EventAttribute>) {
        match self {
            Self::Send(payment) => tags.extend([
                tag("tx.to", &hex::encode(payment.recipient), true),
                tag("tx.coin_id", &payment.coin.to_string(), true),
            ]),
            Self::Multisend(payments) => {
                let recipients: Vec<_> =
                    payments.iter().map(|p| hex::encode(p.recipient)).collect();
                tags.extend(recipients.iter().map(|address| tag("tx.to", address, true)));
                tags.push(tag("tx.to", &recipients.join(","), false));
            }
            Self::CreateCoin(_)
            | Self::CreateToken(_)
            | Self::RecreateCoin(_)
            | Self::RecreateToken(_)
            | Self::PoolTrade(_)
            | Self::Liquidity(_)
            | Self::Bancor(_)
            | Self::Stake(_)
            | Self::Multisig(_)
            | Self::RedeemCheck(_)
            | Self::EditCoinOwner(_)
            | Self::CreatePool(_)
            | Self::LimitOrder(_) => {}
            Self::Supply(change) => tags.push(tag("tx.coin_id", &change.id.to_string(), true)),
            Self::SetHaltBlock(data) => {
                tags.push(tag("tx.public_key", &hex::encode(data.key), true))
            }
            Self::VoteCommission(data) => {
                tags.push(tag("tx.public_key", &hex::encode(data.key), true))
            }
            Self::VoteUpdate(data) => data.tag_success(tags),
        }
    }

    fn type_tag(&self) -> &'static str {
        match self {
            Self::Send(_) => "01",
            Self::Multisend(_) => "0d",
            Self::CreateCoin(_) => "05",
            Self::CreateToken(_) => "1e",
            Self::RecreateCoin(_) => "10",
            Self::RecreateToken(_) => "1f",
            Self::RedeemCheck(_) => "09",
            Self::EditCoinOwner(_) => "11",
            Self::CreatePool(_) => "22",
            Self::SetHaltBlock(_) => "0f",
            Self::Multisig(data) => {
                if data.create {
                    "0c"
                } else {
                    "12"
                }
            }
            Self::VoteCommission(_) => "20",
            Self::VoteUpdate(_) => "21",
            Self::PoolTrade(data) => match data.amount {
                SwapAmount::Sell(_) => "17",
                SwapAmount::Buy(_) => "18",
                SwapAmount::All => "19",
            },
            Self::LimitOrder(data) => match data {
                OrderTransaction::Place(_) => "23",
                OrderTransaction::Cancel(_) => "24",
            },
            Self::Liquidity(data) => match data.kind {
                LiquidityKind::Add { .. } => "15",
                LiquidityKind::Remove { .. } => "16",
            },
            Self::Bancor(data) => match data.kind {
                BancorAmount::Sell(_) => "02",
                BancorAmount::SellAll => "03",
                BancorAmount::Buy(_) => "04",
            },
            Self::Stake(data) => match data {
                StakeTransaction::Declare(_) => "06",
                StakeTransaction::Delegate(_) => "07",
                StakeTransaction::Unbond(_) => "08",
                StakeTransaction::Move(_) => "1b",
                StakeTransaction::Edit(_) => "0e",
                StakeTransaction::EditPublicKey { .. } => "14",
                StakeTransaction::EditCommission { .. } => "1a",
                StakeTransaction::CandidateStatus { online: true, .. } => "0a",
                StakeTransaction::CandidateStatus { online: false, .. } => "0b",
                StakeTransaction::LockStake => "25",
                StakeTransaction::Lock { .. } => "26",
            },
            Self::Supply(change) => {
                if change.mint {
                    "1c"
                } else {
                    "1d"
                }
            }
        }
    }
}

impl Payment {
    fn decode(mut fields: &[u8]) -> Result<Self> {
        let coin = u32::decode(&mut fields)?;
        let recipient = Address::decode(&mut fields)?;
        let value = amount(&mut fields)?;
        finish(fields)?;
        Ok(Self {
            coin,
            recipient,
            value,
        })
    }
}

impl MultisigWitness<'_> {
    fn rejection(&self, sender: &Account, account: Address) -> Option<Outcome> {
        let multisig = &sender.multisig;
        if !sender.is_multisig() {
            return Some(Outcome::reject(
                603,
                "Multisig does not exists".into(),
                [("address", display_address(account))],
            ));
        }
        if self.signatures.len() > 32 || self.signatures.len() > multisig.weights.len() {
            return Some(incorrect_multisig("error in the number of signers"));
        }
        let mut used = BTreeSet::new();
        let mut votes = 0u32;
        for signature in &self.signatures {
            let address = match signature.recover(&self.signing_hash) {
                Ok(address) => address,
                Err(error) => return Some(incorrect_multisig(signature_error(error))),
            };
            if !used.insert(address) {
                return Some(Outcome::reject(
                    606,
                    "Duplicated multisig addresses".into(),
                    [("address", display_address(address))],
                ));
            }
            if let Some(index) = multisig
                .addresses
                .iter()
                .position(|candidate| *candidate == address)
            {
                votes = votes.wrapping_add(multisig.weights[index]);
            }
        }
        let threshold = multisig.threshold;
        (votes < threshold).then(|| {
            Outcome::reject(
                609,
                format!("Not enough multisig votes. Needed {threshold}, has {votes}"),
                [
                    ("needed_votes", threshold.to_string()),
                    ("got_votes", votes.to_string()),
                ],
            )
        })
    }
}

fn incorrect_multisig(message: &str) -> Outcome {
    Outcome::reject(
        604,
        "Incorrect multi-signature".into(),
        [("text", message.into())],
    )
}

fn signature_error(error: InvalidSignature) -> &'static str {
    match error {
        InvalidSignature::Recovery => "recovery failed",
        _ => "invalid transaction v, r, s values",
    }
}

fn unsupported(check: bool, message: String) -> Result<Prepared<'static>> {
    if check {
        Ok(Prepared::Rejected(Outcome::reject(124, message, [])))
    } else {
        bail!("{message}");
    }
}

fn coin_not_found(coin: u32) -> Outcome {
    Outcome::reject(
        102,
        format!("Coin {coin} not exists"),
        [("coin_id", coin.to_string())],
    )
}

fn insufficient(
    address: Address,
    coin: u32,
    symbol: &str,
    needed: &impl std::fmt::Display,
) -> Outcome {
    let address = display_address(address);
    Outcome::reject(
        107,
        format!("Insufficient funds for sender account: {address}. Wanted {needed} {symbol}"),
        [
            ("sender", address),
            ("needed_value", needed.to_string()),
            ("coin_symbol", symbol.into()),
            ("coin_id", coin.to_string()),
        ],
    )
}

pub(crate) fn unpayable_commission() -> Outcome {
    Outcome::reject(119, "Not possible to pay commission".into(), [])
}

pub(crate) fn commission_quote(
    view: &View<'_>,
    pools: Option<&pool::Changes>,
    coin: u32,
    raw: &Amount,
) -> Result<CommissionQuote> {
    let pools = pool::View {
        store: view.store,
        height: view.height,
        changes: pools,
    };
    let price = pool::commission_in_bip(&pools, coin, raw)?;
    if coin != 0 && price.bits() == 0 {
        let symbol = view.symbol(coin)?.context("missing commission coin")?;
        return Ok(CommissionQuote::Rejected(minimum_output(
            coin,
            &symbol,
            view.base_symbol(),
            &Amount::from(1u32),
            &price,
        )));
    }
    Ok(CommissionQuote::Price(price))
}

pub(crate) fn display_address(address: Address) -> String {
    format!("Mx{}", hex::encode(address))
}

fn tag(key: &str, value: &str, index: bool) -> abci::EventAttribute {
    abci::EventAttribute {
        key: key.as_bytes().to_vec().into(),
        value: value.as_bytes().to_vec().into(),
        index,
    }
}
