use super::super::print_address;
use crate::state::{amount, finish, full_list};
use alloy_rlp::{Decodable, Header};
use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use minter_api::{Any, pack, pb};
use minter_protocol::Address;

/// The Go API's v3 decoder renders the submitted RLP independently of
/// execution. This also covers indexed transactions with a nonzero DeliverTx code.
pub(super) fn decode(
    kind: u8,
    input: &[u8],
    coin: impl FnMut(u32) -> Result<pb::Coin>,
) -> Result<(Any, i64)> {
    let mut data = Data {
        fields: full_list(input)?,
        coin,
    };
    let result = data.decode(kind)?;
    finish(data.fields)?;
    Ok(result)
}

struct Data<'a, F> {
    fields: &'a [u8],
    coin: F,
}

impl<F: FnMut(u32) -> Result<pb::Coin>> Data<'_, F> {
    fn decode(&mut self, kind: u8) -> Result<(Any, i64)> {
        Ok(match kind {
            0x01 => (
                pack(pb::SendData {
                    coin: Some(self.coin()?),
                    to: self.address()?,
                    value: self.amount()?,
                }),
                1,
            ),
            0x02 => (
                pack(pb::SellCoinData {
                    coin_to_sell: Some(self.coin()?),
                    value_to_sell: self.amount()?,
                    coin_to_buy: Some(self.coin()?),
                    minimum_value_to_buy: self.amount()?,
                }),
                2,
            ),
            0x03 => (
                pack(pb::SellAllCoinData {
                    coin_to_sell: Some(self.coin()?),
                    coin_to_buy: Some(self.coin()?),
                    minimum_value_to_buy: self.amount()?,
                }),
                2,
            ),
            0x04 => (
                pack(pb::BuyCoinData {
                    coin_to_buy: Some(self.coin()?),
                    value_to_buy: self.amount()?,
                    coin_to_sell: Some(self.coin()?),
                    maximum_value_to_sell: self.amount()?,
                }),
                2,
            ),
            0x05 => (
                pack(pb::CreateCoinData {
                    name: self.read()?,
                    symbol: self.symbol()?,
                    initial_amount: self.amount()?,
                    initial_reserve: self.amount()?,
                    constant_reserve_ratio: u64::from(self.read::<u32>()?),
                    max_supply: self.amount()?,
                }),
                3,
            ),
            0x06 => (
                pack(pb::DeclareCandidacyData {
                    address: self.address()?,
                    pub_key: self.public_key()?,
                    commission: u64::from(self.read::<u32>()?),
                    coin: Some(self.coin()?),
                    stake: self.amount()?,
                }),
                10,
            ),
            0x07 => (
                pack(pb::DelegateData {
                    pub_key: self.public_key()?,
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                6,
            ),
            0x08 => (
                pack(pb::UnbondData {
                    pub_key: self.public_key()?,
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                6,
            ),
            0x09 => (
                pack(pb::RedeemCheckData {
                    raw_check: BASE64.encode(Header::decode_bytes(&mut self.fields, false)?),
                    proof: BASE64.encode(self.read::<[u8; 65]>()?),
                }),
                20,
            ),
            0x0a => (
                pack(pb::SetCandidateOnData {
                    pub_key: self.public_key()?,
                }),
                1,
            ),
            0x0b => (
                pack(pb::SetCandidateOffData {
                    pub_key: self.public_key()?,
                }),
                1,
            ),
            0x0c => (
                pack(pb::CreateMultisigData {
                    threshold: u64::from(self.read::<u32>()?),
                    weights: self
                        .read::<Vec<u32>>()?
                        .into_iter()
                        .map(u64::from)
                        .collect(),
                    addresses: self
                        .read::<Vec<Address>>()?
                        .into_iter()
                        .map(print_address)
                        .collect(),
                }),
                20,
            ),
            0x0d => {
                let list = self.payments()?;
                let gas = 1 + list.len() as i64;
                (pack(pb::MultiSendData { list }), gas)
            }
            0x0e => (
                pack(pb::EditCandidateData {
                    pub_key: self.public_key()?,
                    reward_address: self.address()?,
                    owner_address: self.address()?,
                    control_address: self.address()?,
                }),
                5,
            ),
            0x0f => (
                pack(pb::SetHaltBlockData {
                    pub_key: self.public_key()?,
                    height: self.read()?,
                }),
                5,
            ),
            0x10 => (
                pack(pb::RecreateCoinData {
                    name: self.read()?,
                    symbol: self.symbol()?,
                    initial_amount: self.amount()?,
                    initial_reserve: self.amount()?,
                    constant_reserve_ratio: u64::from(self.read::<u32>()?),
                    max_supply: self.amount()?,
                }),
                5,
            ),
            0x11 => (
                pack(pb::EditCoinOwnerData {
                    symbol: self.symbol()?,
                    new_owner: self.address()?,
                }),
                5,
            ),
            0x12 => (
                pack(pb::EditMultisigData {
                    threshold: u64::from(self.read::<u32>()?),
                    weights: self
                        .read::<Vec<u32>>()?
                        .into_iter()
                        .map(u64::from)
                        .collect(),
                    addresses: self
                        .read::<Vec<Address>>()?
                        .into_iter()
                        .map(print_address)
                        .collect(),
                }),
                5,
            ),
            0x14 => (
                pack(pb::EditCandidatePublicKeyData {
                    pub_key: self.public_key()?,
                    new_pub_key: self.public_key()?,
                }),
                10,
            ),
            0x15 => (
                pack(pb::AddLiquidityData {
                    coin0: Some(self.coin()?),
                    coin1: Some(self.coin()?),
                    volume0: self.amount()?,
                    maximum_volume1: self.amount()?,
                }),
                5,
            ),
            0x16 => (
                pack(pb::RemoveLiquidityData {
                    coin0: Some(self.coin()?),
                    coin1: Some(self.coin()?),
                    liquidity: self.amount()?,
                    minimum_volume0: self.amount()?,
                    minimum_volume1: self.amount()?,
                }),
                5,
            ),
            0x17 => {
                let coins = self.route()?;
                let gas = coins.len() as i64;
                (
                    pack(pb::SellSwapPoolData {
                        coins,
                        value_to_sell: self.amount()?,
                        minimum_value_to_buy: self.amount()?,
                    }),
                    gas,
                )
            }
            0x18 => {
                let coins = self.route()?;
                let gas = coins.len() as i64;
                (
                    pack(pb::BuySwapPoolData {
                        coins,
                        value_to_buy: self.amount()?,
                        maximum_value_to_sell: self.amount()?,
                    }),
                    gas,
                )
            }
            0x19 => {
                let coins = self.route()?;
                let gas = coins.len() as i64;
                (
                    pack(pb::SellAllSwapPoolData {
                        coins,
                        minimum_value_to_buy: self.amount()?,
                    }),
                    gas,
                )
            }
            0x1a => (
                pack(pb::EditCandidateCommission {
                    pub_key: self.public_key()?,
                    commission: u64::from(self.read::<u32>()?),
                }),
                1,
            ),
            0x1b => (
                pack(pb::MoveStakeData {
                    from_pub_key: self.public_key()?,
                    to_pub_key: self.public_key()?,
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                6,
            ),
            0x1c => (
                pack(pb::MintTokenData {
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                1,
            ),
            0x1d => (
                pack(pb::BurnTokenData {
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                1,
            ),
            0x1e => (
                pack(pb::CreateTokenData {
                    name: self.read()?,
                    symbol: self.symbol()?,
                    initial_amount: self.amount()?,
                    max_supply: self.amount()?,
                    mintable: self.read()?,
                    burnable: self.read()?,
                }),
                3,
            ),
            0x1f => (
                pack(pb::RecreateTokenData {
                    name: self.read()?,
                    symbol: self.symbol()?,
                    initial_amount: self.amount()?,
                    max_supply: self.amount()?,
                    mintable: self.read()?,
                    burnable: self.read()?,
                }),
                5,
            ),
            0x20 => {
                let data = pb::VoteCommissionData {
                    pub_key: self.public_key()?,
                    height: self.read()?,
                    coin: Some(self.coin()?),
                    payload_byte: self.amount()?,
                    send: self.amount()?,
                    buy_bancor: self.amount()?,
                    sell_bancor: self.amount()?,
                    sell_all_bancor: self.amount()?,
                    buy_pool_base: self.amount()?,
                    buy_pool_delta: self.amount()?,
                    sell_pool_base: self.amount()?,
                    sell_pool_delta: self.amount()?,
                    sell_all_pool_base: self.amount()?,
                    sell_all_pool_delta: self.amount()?,
                    create_ticker3: self.amount()?,
                    create_ticker4: self.amount()?,
                    create_ticker5: self.amount()?,
                    create_ticker6: self.amount()?,
                    create_ticker7_10: self.amount()?,
                    create_coin: self.amount()?,
                    create_token: self.amount()?,
                    recreate_coin: self.amount()?,
                    recreate_token: self.amount()?,
                    declare_candidacy: self.amount()?,
                    delegate: self.amount()?,
                    unbond: self.amount()?,
                    redeem_check: self.amount()?,
                    set_candidate_on: self.amount()?,
                    set_candidate_off: self.amount()?,
                    create_multisig: self.amount()?,
                    multisend_base: self.amount()?,
                    multisend_delta: self.amount()?,
                    edit_candidate: self.amount()?,
                    set_halt_block: self.amount()?,
                    edit_ticker_owner: self.amount()?,
                    edit_multisig: self.amount()?,
                    edit_candidate_public_key: self.amount()?,
                    create_swap_pool: self.amount()?,
                    add_liquidity: self.amount()?,
                    remove_liquidity: self.amount()?,
                    edit_candidate_commission: self.amount()?,
                    mint_token: self.amount()?,
                    burn_token: self.amount()?,
                    vote_commission: self.amount()?,
                    vote_update: self.amount()?,
                    failed_tx: self.amount()?,
                    add_limit_order: self.amount()?,
                    remove_limit_order: self.amount()?,
                    move_stake: self.amount()?,
                    lock_stake: self.amount()?,
                    lock: self.amount()?,
                };
                // VoteCommissionDataV3 permits a tail of future prices.
                while !self.fields.is_empty() {
                    amount(&mut self.fields)?;
                }
                (pack(data), 5)
            }
            0x21 => (
                pack(pb::VoteUpdateData {
                    version: self.read()?,
                    pub_key: self.public_key()?,
                    height: self.read()?,
                }),
                5,
            ),
            0x22 => (
                pack(pb::CreateSwapPoolData {
                    coin0: Some(self.coin()?),
                    coin1: Some(self.coin()?),
                    volume0: self.amount()?,
                    volume1: self.amount()?,
                }),
                10,
            ),
            0x23 => (
                pack(pb::AddLimitOrderData {
                    coin_to_sell: Some(self.coin()?),
                    value_to_sell: self.amount()?,
                    coin_to_buy: Some(self.coin()?),
                    value_to_buy: self.amount()?,
                }),
                50,
            ),
            0x24 => (
                pack(pb::RemoveLimitOrderData {
                    id: u64::from(self.read::<u32>()?),
                }),
                50,
            ),
            0x25 => (pack(pb::LockStakeData {}), 2),
            0x26 => (
                pack(pb::LockData {
                    due_block: u64::from(self.read::<u32>()?),
                    coin: Some(self.coin()?),
                    value: self.amount()?,
                }),
                2,
            ),
            _ => bail!("unknown tx type"),
        })
    }

    fn read<T: Decodable>(&mut self) -> Result<T> {
        Ok(T::decode(&mut self.fields)?)
    }

    fn coin(&mut self) -> Result<pb::Coin> {
        let id = self.read::<u32>()?;
        (self.coin)(id)
    }

    fn route(&mut self) -> Result<Vec<pb::Coin>> {
        self.read::<Vec<u32>>()?
            .into_iter()
            .map(&mut self.coin)
            .collect()
    }

    fn amount(&mut self) -> Result<String> {
        Ok(amount(&mut self.fields)?.to_string())
    }

    fn public_key(&mut self) -> Result<String> {
        Ok(format!("Mp{}", hex::encode(self.read::<[u8; 32]>()?)))
    }

    fn address(&mut self) -> Result<String> {
        Ok(print_address(self.read()?))
    }

    fn symbol(&mut self) -> Result<String> {
        let bytes = self.read::<[u8; 10]>()?;
        Ok(std::str::from_utf8(&bytes)?.trim_matches('\0').to_owned())
    }

    fn payments(&mut self) -> Result<Vec<pb::SendData>> {
        let mut list = Header::decode_bytes(&mut self.fields, true)?;
        let mut payments = Vec::new();
        while !list.is_empty() {
            let mut item = Data {
                fields: Header::decode_bytes(&mut list, true)?,
                coin: &mut self.coin,
            };
            payments.push(pb::SendData {
                coin: Some(item.coin()?),
                to: item.address()?,
                value: item.amount()?,
            });
            finish(item.fields)?;
        }
        Ok(payments)
    }
}
