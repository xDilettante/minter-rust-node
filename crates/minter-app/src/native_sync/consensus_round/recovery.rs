use super::*;

const MAX_SAVED_CERTIFICATES: usize = 4;

#[derive(Clone, PartialEq, Message)]
pub(super) struct SavedPrevotes {
    #[prost(int32, tag = "1")]
    round: i32,
    #[prost(message, repeated, tag = "2")]
    votes: Vec<types::Vote>,
    #[prost(bytes, tag = "3")]
    block: Vec<u8>,
}

impl Round {
    pub(super) fn saved(
        &self,
        step: u32,
        proposal: Option<&types::Proposal>,
        block: Option<&ReceivedBlock>,
    ) -> Vec<u8> {
        let mut state = SavedRound {
            height: self.height,
            round: self.number,
            step,
            proposal: proposal.cloned(),
            proposal_block: block.map_or_else(Vec::new, |block| block.wire().to_vec()),
            ..Default::default()
        };
        if let Some((round, id, time)) = self.locked {
            state.locked_round = round;
            state.locked_block = Some(id.into());
            state.locked_time = Some(time.into());
            if block.is_none_or(|block| block.id() != id)
                && let Some(parts) = self.parts(id.part_set_header)
                && let Ok(Some(block)) = parts.block()
            {
                state.locked_block_bytes = block.wire().to_vec();
            }
        }
        // Preserve the certificates needed to keep a lock, repropose, or unlock.
        // Local round history remains in memory; the durable subset is at most four.
        let numbers: BTreeSet<_> = [
            self.locked.map(|(round, _, _)| round),
            proposal.map(|p| p.pol_round).filter(|r| *r >= 0),
            self.best_valid_block().map(|(round, _)| round),
            self.votes
                .range(..=self.number)
                .rev()
                .find_map(|(&n, v)| v.prevotes.quorum.map(|_| n)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut stored_blocks: BTreeSet<_> = block.map(ReceivedBlock::id).into_iter().collect();
        if !state.locked_block_bytes.is_empty()
            && let Some((_, id, _)) = self.locked
        {
            stored_blocks.insert(id);
        }
        for number in numbers {
            let Some(pool) = self.votes.get(&number).map(|v| &v.prevotes) else {
                continue;
            };
            let Some(id) = pool.quorum else { continue };
            let block = id
                .filter(|id| stored_blocks.insert(*id))
                .and_then(|id| {
                    self.parts(id.part_set_header)?
                        .block()
                        .ok()
                        .flatten()
                        .filter(|block| block.id() == id)
                })
                .map_or_else(Vec::new, |block| block.wire().to_vec());
            state.prevote_certificates.push(SavedPrevotes {
                round: number,
                votes: pool
                    .canonical()
                    .filter(|(_, vote)| vote.block_id == id)
                    .map(|(_, vote)| vote.clone().into())
                    .collect(),
                block,
            });
        }
        state.encode_to_vec()
    }

    pub(super) fn recover(&mut self, signer: &ValidatorSigner) -> Result<()> {
        let (height, number, step) = signer.position();
        ensure!(
            height <= self.height,
            "signing journal is ahead of the node"
        );
        if height != self.height {
            return Ok(());
        }
        ensure!(number >= 0, "persisted consensus round is negative");
        let bytes = signer
            .consensus_round()?
            .context("uncommitted signature has no native consensus journal")?;
        let saved = SavedRound::decode(bytes.as_slice())?;
        let expected = match step {
            validator_signer::PROPOSE => PROPOSE,
            validator_signer::PREVOTE => PREVOTE,
            validator_signer::PRECOMMIT => PRECOMMIT,
            _ => anyhow::bail!("invalid consensus signing step"),
        };
        ensure!(
            saved.height == height && saved.round == number && saved.step == expected,
            "native consensus journal differs from the last signature"
        );
        self.enter_propose(number, Instant::now())?;
        let index = self
            .local_validator(signer)
            .context("recovered key is not a validator")?;
        ensure!(
            saved.prevote_certificates.len() <= MAX_SAVED_CERTIFICATES,
            "too many persisted prevote certificates"
        );
        let mut restored = BTreeSet::new();
        for certificate in saved.prevote_certificates {
            ensure!(
                restored.insert(certificate.round),
                "duplicate persisted certificate round"
            );
            self.restore_certificate(certificate)?;
        }
        if let Some(id) = saved.locked_block {
            ensure!(
                (0..=number).contains(&saved.locked_round),
                "invalid consensus lock round"
            );
            let id: block::Id = id.try_into()?;
            let time = saved
                .locked_time
                .context("missing locked block time")?
                .try_into()?;
            self.locked = Some((saved.locked_round, id, time));
            if !saved.locked_block_bytes.is_empty() {
                self.restore_parts(id, saved.locked_block_bytes)?;
            }
        }
        if let Some(mut proposal) = saved.proposal {
            let id = proposal
                .block_id
                .clone()
                .context("missing saved proposal block ID")?;
            if expected == PROPOSE {
                ensure!(
                    !saved.proposal_block.is_empty(),
                    "missing signed proposal block"
                );
                proposal = signer.signed_proposal(&id)?;
            }
            self.receive_proposal(proposal)?;
            if !saved.proposal_block.is_empty() {
                self.restore_parts(id.try_into()?, saved.proposal_block)?;
            }
        } else {
            ensure!(expected != PROPOSE, "missing signed proposal");
        }
        if expected != PROPOSE {
            let vote = signer.signed_vote(index.try_into()?)?;
            let id = parse_vote(vote.clone(), account::Id::from(signer.public_key()))?.block_id;
            if expected == PRECOMMIT
                && let Some(id) = id
            {
                ensure!(
                    self.locked
                        .is_some_and(|(round, locked, _)| round == number && locked == id),
                    "persisted lock differs from the last precommit"
                );
            }
            self.receive_vote(vote, None)?;
        }
        self.step = expected;
        println!("Restored Tendermint signing state at {height}/{number} step {expected}");
        Ok(())
    }

    fn restore_certificate(&mut self, certificate: SavedPrevotes) -> Result<()> {
        let number = certificate.round;
        ensure!(
            (0..=self.number).contains(&number),
            "invalid persisted certificate round"
        );
        ensure!(
            !certificate.votes.is_empty()
                && certificate.votes.len() <= self.validators.validators.len(),
            "invalid persisted certificate vote count"
        );
        let mut seen = BTreeSet::new();
        for vote in certificate.votes {
            ensure!(
                vote.round == number
                    && vote.r#type == SignedMsgType::Prevote as i32
                    && seen.insert(vote.validator_index),
                "invalid persisted certificate vote"
            );
            self.receive_vote(vote, None)?;
        }
        let pool = &self.votes[&number].prevotes;
        let id = pool.quorum.context("persisted prevotes lack a quorum")?;
        ensure!(
            pool.power.len() == 1,
            "persisted certificate names different blocks"
        );
        self.votes.get_mut(&number).unwrap().local = true;
        if !certificate.block.is_empty() {
            self.restore_parts(
                id.context("nil certificate includes a block")?,
                certificate.block,
            )?;
        }
        Ok(())
    }

    pub(super) fn restore_parts(&mut self, id: block::Id, bytes: Vec<u8>) -> Result<()> {
        let block = ReceivedBlock::decode(bytes)?;
        ensure!(
            block.id() == id && block.encoded_len() <= self.max_block_bytes,
            "persisted block differs from the signed block ID or size limit"
        );
        self.set_parts(BlockParts::from_block(block)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{high_round_reactor, high_round_vote};
    use super::*;

    #[test]
    fn accepts_only_verified_current_height_prevote_certificates() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let reactor = high_round_reactor(temp.path())?;
        let valid = SavedPrevotes {
            round: 64,
            votes: (0..3)
                .map(|i| high_round_vote(&reactor, i, 64, Type::Prevote, None))
                .collect::<Result<_>>()?,
            block: Vec::new(),
        };
        let restore = |certificate| -> Result<Round> {
            let mut round = NativeConsensus::next_round(&reactor.node)?;
            round.enter_propose(65, Instant::now())?;
            round.restore_certificate(certificate)?;
            Ok(round)
        };
        assert_eq!(
            restore(valid.clone())?.votes[&64].prevotes.quorum,
            Some(None)
        );
        let mut cases = Vec::new();
        let mut bad = valid.clone();
        bad.votes[0].signature[0] ^= 1;
        cases.push(bad);
        let mut bad = valid.clone();
        bad.votes[0].height += 1;
        cases.push(bad);
        let mut bad = valid.clone();
        bad.votes[0] = bad.votes[1].clone();
        cases.push(bad);
        let mut bad = valid.clone();
        bad.votes.pop();
        cases.push(bad);
        let mut bad = valid.clone();
        bad.round = 66;
        cases.push(bad);
        let mut bad = valid.clone();
        bad.votes[0] = high_round_vote(&reactor, 0, 64, Type::Precommit, None)?;
        cases.push(bad);
        let mut bad = valid;
        bad.block = vec![1];
        cases.push(bad);
        for (index, bad) in cases.into_iter().enumerate() {
            assert!(
                restore(bad).is_err(),
                "accepted invalid certificate case {index}"
            );
        }
        Ok(())
    }
}
