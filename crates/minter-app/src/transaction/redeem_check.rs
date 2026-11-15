use super::{
    GasPayment, Outcome, Transaction, TransactionState, display_address, signature_error, tag,
};
use crate::{
    coins::View,
    state::{Account, Amount, finish},
};
use alloy_rlp::{Decodable, Header};
use anyhow::{Context, Result};
use minter_protocol::{Address, SigningHash, check::Check, state_keys::used_check_key};
use tendermint_proto::v0_34::abci::EventAttribute;

pub(crate) struct RedeemCheck {
    raw: Vec<u8>,
    proof: [u8; 65],
}

pub(crate) struct Redemption {
    pub hash: SigningHash,
    pub issuer: Address,
    pub coin: u32,
    pub amount: Amount,
}

impl RedeemCheck {
    pub fn decode(mut fields: &[u8]) -> Result<Self> {
        let raw = Header::decode_bytes(&mut fields, false)?.to_vec();
        let proof = <[u8; 65]>::decode(&mut fields)?;
        finish(fields)?;
        Ok(Self { raw, proof })
    }

    pub fn plan(
        &self,
        coins: &View<'_>,
        state: &TransactionState<'_>,
        tx: &Transaction<'_>,
    ) -> Result<Result<Redemption, Outcome>> {
        if self.raw.is_empty() {
            return Ok(Err(Outcome::reject(106, "Incorrect tx data".into(), [])));
        }
        if tx.gas_price != 1 {
            return Ok(Err(Outcome::reject(
                504,
                "Gas price for check is limited to 1".into(),
                [
                    ("max_check_gas_price", "1".into()),
                    ("current_gas_price", tx.gas_price.to_string()),
                ],
            )));
        }
        let check = match self.check() {
            Ok(check) => check,
            Err(outcome) => return Ok(Err(outcome)),
        };
        if check.chain_id != coins.network {
            // The Go error reports the outer transaction's chain ID.
            let chain = coins.network.to_string();
            return Ok(Err(Outcome::reject(
                115,
                "Wrong chain id".into(),
                [("current_chain_id", chain.clone()), ("got_chain_id", chain)],
            )));
        }
        if check.nonce.len() > 16 {
            return Ok(Err(Outcome::reject(
                506,
                "Nonce is too big. Should be up to 16 bytes.".into(),
                [
                    ("nonce_bytes", check.nonce.len().to_string()),
                    ("max_nonce_bytes", "16".into()),
                ],
            )));
        }
        let issuer = match issuer(&check) {
            Ok(issuer) => issuer,
            Err(outcome) => return Ok(Err(outcome)),
        };
        for (id, message) in [
            (check.coin, "Coin not exists"),
            (check.gas_coin, "Gas coin not exists"),
        ] {
            if coins.symbol(id)?.is_none() {
                return Ok(Err(Outcome::reject(
                    102,
                    message.into(),
                    [("coin_id", id.to_string())],
                )));
            }
        }
        if tx.gas_coin != check.gas_coin {
            return Ok(Err(Outcome::reject(
                505,
                format!(
                    "CommissionData coin for redeem check transaction can only be {}",
                    check.gas_coin
                ),
                [
                    (
                        "tx_coin_symbol",
                        coins
                            .symbol(tx.gas_coin)?
                            .context("missing transaction gas coin")?,
                    ),
                    ("tx_coin_id", tx.gas_coin.to_string()),
                    (
                        "check_coin_symbol",
                        coins
                            .symbol(check.gas_coin)?
                            .context("missing check gas coin")?,
                    ),
                    ("check_coin_id", check.gas_coin.to_string()),
                ],
            )));
        }
        let height = coins.height + 1;
        if check.due_block < height {
            return Ok(Err(Outcome::reject(
                502,
                "Check expired".into(),
                [
                    ("due_block", check.due_block.to_string()),
                    ("current_block", height.to_string()),
                ],
            )));
        }
        let hash = check.hash();
        if state
            .used_checks
            .is_some_and(|checks| checks.contains(&hash))
            || coins
                .store
                .value_at(coins.height, &used_check_key(hash))?
                .is_some_and(|row| !row.is_empty())
        {
            return Ok(Err(Outcome::reject(
                503,
                "Check already redeemed".into(),
                [],
            )));
        }
        match check.verify_recipient(tx.sender, &self.proof) {
            Ok(true) => {}
            Ok(false) => return Ok(Err(Outcome::reject(501, "Invalid proof".into(), []))),
            Err(error) => return Ok(Err(Outcome::reject(106, error.to_string(), []))),
        }
        Ok(Ok(Redemption {
            hash,
            issuer,
            coin: check.coin,
            amount: Amount::from_bytes_be(check.value),
        }))
    }
    fn check(&self) -> Result<Check<'_>, Outcome> {
        if self.raw.is_empty() {
            return Err(Outcome::reject(106, "EOF".into(), []));
        }
        Check::decode(&self.raw).map_err(|error| Outcome::reject(106, error.to_string(), []))
    }

    pub fn issuer(&self) -> Result<Address, Outcome> {
        issuer(&self.check()?)
    }
}

fn issuer(check: &Check<'_>) -> Result<Address, Outcome> {
    check
        .issuer()
        .map_err(|error| Outcome::reject(106, signature_error(error).into(), []))
}

impl Redemption {
    pub fn balance_rejection(
        &self,
        coins: &View<'_>,
        issuer: &mut Account,
        recipient: Address,
        gas: &GasPayment,
    ) -> Result<Option<Outcome>> {
        if self.coin == gas.coin() {
            let total = &self.amount + gas.amount();
            if issuer.balance(coins.store, coins.height, self.coin)? < &total {
                let symbol = coins.symbol(self.coin)?.context("missing check coin")?;
                return Ok(Some(insufficient(
                    recipient,
                    self.coin,
                    &symbol,
                    &total,
                    format!("{} {symbol}", self.amount),
                )));
            }
            return Ok(None);
        }
        if issuer.balance(coins.store, coins.height, self.coin)? < &self.amount {
            let symbol = coins.symbol(self.coin)?.context("missing check coin")?;
            return Ok(Some(insufficient(
                self.issuer,
                self.coin,
                &symbol,
                &self.amount,
                format!("{} {}", self.amount, self.coin),
            )));
        }
        if issuer.balance(coins.store, coins.height, gas.coin())? < gas.amount() {
            let symbol = coins
                .symbol(gas.coin())?
                .context("missing check gas coin")?;
            return Ok(Some(insufficient(
                recipient,
                gas.coin(),
                &symbol,
                gas.amount(),
                format!("{} {}", self.amount, gas.coin()),
            )));
        }
        Ok(None)
    }

    pub fn tag_success(&self, recipient: Address, tags: &mut Vec<EventAttribute>) {
        tags.extend([
            tag("tx.to", &hex::encode(recipient), true),
            tag("tx.coin_id", &self.coin.to_string(), true),
            tag("tx.from", &hex::encode(self.issuer), true),
        ]);
    }
}

fn insufficient(
    account: Address,
    coin: u32,
    symbol: &str,
    needed: &Amount,
    value: String,
) -> Outcome {
    Outcome::reject(
        107,
        format!("Insufficient funds for check issuer account: {value}. Wanted {needed} {symbol}"),
        [
            ("sender", display_address(account)),
            ("needed_value", needed.to_string()),
            ("coin_symbol", symbol.into()),
            ("coin_id", coin.to_string()),
        ],
    )
}
