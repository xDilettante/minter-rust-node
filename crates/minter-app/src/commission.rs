use crate::state::{Amount, amount, full_list, list, put_amount};
use alloy_rlp::{Decodable, Encodable};
use anyhow::{Context, Result};
use minter_api::pb::{Coin, PriceCommissionResponse, UpdateCommissionsEvent};
use std::collections::BTreeMap;

// The same ordered values form the IAVL price table and the Minter v2 views.
macro_rules! fee_fields {
    ($($name:ident),+ $(,)?) => {
        pub(crate) struct Fees {
            pub price_coin: u32,
            $(pub $name: Amount,)+
            pub more: Vec<Amount>,
        }

        impl Fees {
            pub fn from_genesis(prices: &BTreeMap<String, String>) -> Result<Self> {
                let field = |name| {
                    prices.get(name).with_context(|| format!("missing commission {name}"))
                };
                Ok(Self {
                    price_coin: field("coin")?.parse()?,
                    $($name: field(stringify!($name))?.parse()?,)+
                    more: Vec::new(),
                })
            }

            pub fn decode(raw: &[u8]) -> Result<Self> {
                Self::decode_fields(full_list(raw)?)
            }

            pub fn decode_fields(mut fields: &[u8]) -> Result<Self> {
                let mut prices = Self {
                    price_coin: u32::decode(&mut fields)?,
                    $($name: amount(&mut fields)?,)+
                    more: Vec::new(),
                };
                while !fields.is_empty() {
                    prices.more.push(amount(&mut fields)?);
                }
                Ok(prices)
            }

            pub fn encode(&self) -> Vec<u8> {
                list(|out| {
                    self.price_coin.encode(out);
                    $(put_amount(&self.$name, out);)+
                    for extra in &self.more {
                        put_amount(extra, out);
                    }
                })
            }

            pub fn event(&self) -> UpdateCommissionsEvent {
                UpdateCommissionsEvent {
                    coin: u64::from(self.price_coin),
                    $($name: self.$name.to_string(),)+
                }
            }

            pub fn response(&self, symbol: String) -> PriceCommissionResponse {
                PriceCommissionResponse {
                    coin: Some(Coin {
                        id: u64::from(self.price_coin),
                        symbol,
                    }),
                    $($name: self.$name.to_string(),)+
                }
            }
        }
    };
}

fee_fields!(
    payload_byte,
    send,
    buy_bancor,
    sell_bancor,
    sell_all_bancor,
    buy_pool_base,
    buy_pool_delta,
    sell_pool_base,
    sell_pool_delta,
    sell_all_pool_base,
    sell_all_pool_delta,
    create_ticker3,
    create_ticker4,
    create_ticker5,
    create_ticker6,
    create_ticker7_10,
    create_coin,
    create_token,
    recreate_coin,
    recreate_token,
    declare_candidacy,
    delegate,
    unbond,
    redeem_check,
    set_candidate_on,
    set_candidate_off,
    create_multisig,
    multisend_base,
    multisend_delta,
    edit_candidate,
    set_halt_block,
    edit_ticker_owner,
    edit_multisig,
    edit_candidate_public_key,
    create_swap_pool,
    add_liquidity,
    remove_liquidity,
    edit_candidate_commission,
    burn_token,
    mint_token,
    vote_commission,
    vote_update,
    failed_tx,
    add_limit_order,
    remove_limit_order,
    move_stake,
    lock_stake,
    lock,
);

impl Fees {
    pub fn ticker_price(&self, length: usize) -> &Amount {
        match length {
            3 => &self.create_ticker3,
            4 => &self.create_ticker4,
            5 => &self.create_ticker5,
            6 => &self.create_ticker6,
            _ => &self.create_ticker7_10,
        }
    }
}
