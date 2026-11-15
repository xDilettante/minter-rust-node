use crate::{
    Block, bancor, frozen, stakes,
    state::{Amount, PublicKey, display_key},
    transaction::display_address,
};
use anyhow::{Context, Result, ensure};
use minter_api::{pack, pb::SlashEvent};
use minter_protocol::Address;
use minter_state::BlockStore;
use tendermint_proto::v0_34::abci;

impl Block {
    pub(super) fn punish_evidence(
        &mut self,
        store: &BlockStore,
        evidence: Vec<abci::Evidence>,
    ) -> Result<()> {
        if evidence.is_empty() {
            return Ok(());
        }
        let mut candidates = match self.candidates.take() {
            Some(candidates) => candidates,
            None => stakes::Candidates::load(
                store,
                self.prior_height,
                &self.chain.retained_candidate_rows,
            )?,
        };
        let mut funds = std::mem::take(&mut self.frozen);
        let unlock = self.height + frozen::unbond_period(self.chain.transaction_chain_id);
        let heights = self.height..=unlock;
        let mut staged = false;

        // Tendermint supplies verified evidence. Go applies the same penalty
        // for both kinds, in the order they appear in BeginBlock.
        for item in evidence {
            let validator = item.validator.context("evidence lacks a validator")?;
            let address: Address = validator.address.as_ref().try_into()?;
            let Some(index) = self.validators.iter().position(|v| v.address() == address) else {
                continue;
            };
            let key = self.validators[index].key;
            let Some(candidate) = candidates.find_mut(key).filter(|c| !c.is_offline()) else {
                continue;
            };
            let id = candidate.id();
            if !staged {
                funds.stage_range(store, self.prior_height, heights.clone())?;
                staged = true;
            }
            for fund in funds.candidate_funds(heights.clone(), id) {
                let source = fund
                    .source
                    .context("frozen stake lacks a source candidate")?;
                fund.value = self.slash_stake(store, fund.owner, fund.coin, source, &fund.value)?;
            }

            let validator = &mut self.validators[index];
            validator.stake = Amount::ZERO;
            validator.dirty = true;
            self.dropped_validators.insert(key);

            for stake in candidate.stakes.iter_mut().flatten() {
                let value = stake.take_value();
                let remaining = self.slash_stake(store, stake.owner, stake.coin, key, &value)?;
                funds.push_reference(
                    store,
                    self.prior_height,
                    unlock,
                    frozen::Fund::new(stake.owner, stake.coin, remaining, Some((key, id)), 0),
                )?;
            }
        }

        self.candidates = Some(candidates);
        self.frozen = funds;
        Ok(())
    }

    fn slash_stake(
        &mut self,
        store: &BlockStore,
        owner: Address,
        coin: u32,
        key: PublicKey,
        value: &Amount,
    ) -> Result<Amount> {
        let remaining = value * 95u32 / 100u32;
        let slashed = value - &remaining;
        let bip = if coin == 0 {
            slashed.clone()
        } else {
            let view = self.coins_view(store);
            let ratio = view.coin(coin)?.context("unknown slashed coin")?.ratio;
            let mut supply = view.supply(coin)?.into_owned();
            let bip = bancor::sale_return(&supply.volume, &supply.reserve, ratio, &slashed)?;
            ensure!(bip <= supply.reserve, "slash exceeds coin reserve");
            supply.volume -= &slashed;
            supply.reserve -= &bip;
            self.coins.set_supply(coin, supply);
            bip
        };
        self.app.dirty |= bip.bits() != 0;
        self.app.slashed += bip;
        self.events.push(pack(SlashEvent {
            address: display_address(owner),
            amount: slashed.to_string(),
            coin: u64::from(coin),
            validator_pub_key: display_key(key),
        }));
        Ok(remaining)
    }
}
