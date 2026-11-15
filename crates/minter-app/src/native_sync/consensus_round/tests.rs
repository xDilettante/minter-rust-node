use super::*;
use crate::native_sync::NativeSync;
use ed25519_consensus::SigningKey;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use tendermint::{PublicKey, private_key::PrivateKey};
use tendermint_config::PrivValidatorKey;
use tendermint_proto::v0_34::abci::{Request, request};

#[test]
fn advances_past_round_sixty_four() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let genesis = temp.path().join("genesis.json");
    fs::write(
        &genesis,
        include_bytes!("../../../tests/fixtures/native-four-validator-genesis.json"),
    )?;
    let reactor = NativeConsensus::new(
        NativeNode::new(NativeSync::open(
            &temp.path().join("state"),
            1,
            Some(&genesis),
            None,
        )?),
        None,
    )?;
    let mut round = reactor.lock()?;
    round.enter_propose(64, Instant::now())?;
    assert_eq!(round.number, 64);
    round.enter_propose(65, Instant::now())?;
    assert_eq!(round.number, 65);
    Ok(())
}

#[test]
fn collects_a_distant_round_commit_from_a_peer() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let genesis = temp.path().join("genesis.json");
    fs::write(
        &genesis,
        include_bytes!("../../../tests/fixtures/native-four-validator-genesis.json"),
    )?;
    let reactor = NativeConsensus::new(
        NativeNode::new(NativeSync::open(
            &temp.path().join("state"),
            1,
            Some(&genesis),
            None,
        )?),
        None,
    )?;
    let validators = reactor.lock()?.validators.clone();
    let keys: Vec<SigningKey> = ["majority", "minority", "third", "fourth"]
        .into_iter()
        .map(|role| {
            SigningKey::from(<[u8; 32]>::from(Sha256::digest(format!(
                "public Minter evidence fixture {role}"
            ))))
        })
        .collect();
    let id = block::Id {
        hash: tendermint::Hash::Sha256([3; 32]),
        part_set_header: block::parts::Header::new(1, tendermint::Hash::Sha256([4; 32]))?,
    };
    let peer = "1234567890123456789012345678901234567890".parse()?;
    for (index, validator) in validators.validators.iter().take(3).enumerate() {
        let key = keys
            .iter()
            .find(|key| PublicKey::from(key.verification_key()) == validator.pub_key)
            .context("key")?;
        let domain = Vote {
            vote_type: Type::Precommit,
            height: 101u32.into(),
            round: 100u16.into(),
            block_id: Some(id),
            timestamp: Some(Time::now()),
            validator_address: validator.address,
            validator_index: index.try_into()?,
            signature: None,
            extension: Vec::new(),
            extension_signature: None,
        };
        let canonical: types::CanonicalVote =
            tendermint::vote::CanonicalVote::new(domain.clone(), reactor.lock()?.chain.clone())
                .into();
        let mut wire: types::Vote = domain.into();
        wire.signature = key
            .sign(&canonical.encode_length_delimited_to_vec())
            .to_bytes()
            .to_vec();
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(wire) }))?;
    }
    assert_eq!(reactor.lock()?.certified_block(), Some((100, id)));
    Ok(())
}

pub(super) fn high_round_reactor(temp: &std::path::Path) -> Result<NativeConsensus> {
    let genesis = temp.join("genesis.json");
    fs::write(
        &genesis,
        include_bytes!("../../../tests/fixtures/native-four-validator-genesis.json"),
    )?;
    NativeConsensus::new(
        NativeNode::new(NativeSync::open(
            &temp.join("state"),
            1,
            Some(&genesis),
            None,
        )?),
        None,
    )
}

pub(super) fn high_round_vote(
    reactor: &NativeConsensus,
    index: usize,
    number: i32,
    kind: Type,
    id: Option<block::Id>,
) -> Result<types::Vote> {
    let round = reactor.lock()?;
    let validator = &round.validators.validators[index];
    let key = ["majority", "minority", "third", "fourth"]
        .into_iter()
        .map(|role| {
            SigningKey::from(<[u8; 32]>::from(Sha256::digest(format!(
                "public Minter evidence fixture {role}"
            ))))
        })
        .find(|key| PublicKey::from(key.verification_key()) == validator.pub_key)
        .context("key")?;
    let domain = Vote {
        vote_type: kind,
        height: round.height.try_into()?,
        round: number.try_into()?,
        block_id: id,
        timestamp: Some(Time::now()),
        validator_address: validator.address,
        validator_index: index.try_into()?,
        signature: None,
        extension: Vec::new(),
        extension_signature: None,
    };
    let canonical: types::CanonicalVote =
        tendermint::vote::CanonicalVote::new(domain.clone(), round.chain.clone()).into();
    let mut wire: types::Vote = domain.into();
    wire.signature = key
        .sign(&canonical.encode_length_delimited_to_vec())
        .to_bytes()
        .to_vec();
    Ok(wire)
}

fn high_round_signer(root: &std::path::Path, key_round: i32) -> Result<NativeConsensus> {
    fs::create_dir(root)?;
    let preview = high_round_reactor(root)?;
    let (address, chain) = {
        let mut round = preview.lock()?;
        round.enter_propose(key_round, Instant::now())?;
        (round.selected_proposer(key_round)?, round.chain.clone())
    };
    drop(preview);
    let signing_key = ["majority", "minority", "third", "fourth"]
        .into_iter()
        .map(|role| {
            SigningKey::from(<[u8; 32]>::from(Sha256::digest(format!(
                "public Minter evidence fixture {role}"
            ))))
        })
        .find(|key| account::Id::from(PublicKey::from(key.verification_key())) == address)
        .context("fixture proposer key")?;
    fs::write(
        root.join("test-key.json"),
        serde_json::to_vec(&PrivValidatorKey {
            address,
            pub_key: PublicKey::from(signing_key.verification_key()),
            priv_key: PrivateKey::from(signing_key),
        })?,
    )?;
    fs::write(
        root.join("go-state.json"),
        br#"{"height":"0","round":0,"step":0}"#,
    )?;
    ValidatorSigner::import(
        &root.join("signer"),
        &root.join("test-key.json"),
        &root.join("go-state.json"),
        chain,
    )?;
    reopen_high_round_signer(root)
}

fn reopen_high_round_signer(root: &std::path::Path) -> Result<NativeConsensus> {
    let genesis = root.join("genesis.json");
    let node = NativeNode::new(NativeSync::open(
        &root.join("state"),
        1,
        Some(&genesis),
        None,
    )?);
    let chain = node.lock()?.chain_id().clone();
    NativeConsensus::new(
        node,
        Some(ValidatorSigner::open(
            &root.join("signer"),
            &root.join("test-key.json"),
            &chain,
        )?),
    )
}

fn complete_high_round(reactor: &NativeConsensus, number: i32, id: block::Id) -> Result<()> {
    let peer = "1234567890123456789012345678901234567890".parse()?;
    for kind in [Type::Prevote, Type::Precommit] {
        let votes = (0..4)
            .map(|index| high_round_vote(reactor, index, number, kind, Some(id)))
            .collect::<Result<Vec<_>>>()?;
        for vote in votes {
            reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
        }
    }
    Ok(())
}

#[test]
fn restores_round_sixty_five_with_the_previous_rounds_certificate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let maker = high_round_signer(&temp.path().join("maker"), 64)?;
    let root = temp.path().join("target");
    let target = high_round_signer(&root, 65)?;
    let peer = "1234567890123456789012345678901234567890".parse()?;
    let (proposal, parts, id) = {
        maker.lock()?.enter_propose(64, Instant::now())?;
        maker.advance_local(&mut *maker.lock()?)?;
        let round = maker.lock()?;
        let id = round.block_for_round(64).context("proposal block")?;
        let parts = round.parts(id.part_set_header).context("parts")?;
        (
            round.proposals[&64].clone(),
            (0..parts.header().total as usize)
                .map(|i| parts.part(i).unwrap().clone())
                .collect::<Vec<_>>(),
            id,
        )
    };
    target.lock()?.enter_propose(64, Instant::now())?;
    target.receive(
        peer,
        Sum::Proposal(cs::Proposal {
            proposal: Some(proposal),
        }),
    )?;
    for part in parts {
        target.receive(
            peer,
            Sum::BlockPart(cs::BlockPart {
                height: 101,
                round: 64,
                part: Some(part),
            }),
        )?;
    }
    for index in 0..4 {
        let vote = high_round_vote(&target, index, 64, Type::Prevote, Some(id))?;
        target.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
    }
    assert_eq!(target.lock()?.locked.context("lock")?.0, 64);
    target.lock()?.enter_propose(65, Instant::now())?;
    target.advance_local(&mut *target.lock()?)?;
    assert_eq!(target.lock()?.proposals[&65].pol_round, 64);
    drop(target);
    let recovered = reopen_high_round_signer(&root)?;
    {
        let round = recovered.lock()?;
        assert_eq!(round.number, 65);
        assert_eq!(round.step, PREVOTE);
        assert_eq!(round.locked.context("restored lock")?.1, id);
        assert_eq!(
            round.votes.get(&64).and_then(|v| v.prevotes.quorum),
            Some(Some(id))
        );
        assert_eq!(
            recovered
                .valid_proposal(&round, 65)?
                .context("restored proposal")?
                .0,
            id
        );
    }
    complete_high_round(&recovered, 65, id)?;
    assert_eq!(recovered.node.lock()?.head()?.height, 101);
    assert_eq!(
        recovered
            .lock()?
            .last_commit
            .as_ref()
            .context("commit")?
            .round
            .value(),
        65
    );
    for height in 102..=106 {
        let (number, id) = {
            let mut round = recovered.lock()?;
            let address = account::Id::from(
                recovered
                    .signer
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .public_key(),
            );
            let mut number = 0;
            while round.selected_proposer(number)? != address {
                number += 1;
                ensure!(number < 4, "fixture proposer schedule did not cycle");
                round.enter_propose(number, Instant::now())?;
            }
            round.enter_propose(number, Instant::now())?;
            recovered.advance_local(&mut round)?;
            (
                number,
                round
                    .block_for_round(number)
                    .context("following proposal")?,
            )
        };
        complete_high_round(&recovered, number, id)?;
        assert_eq!(recovered.node.lock()?.head()?.height, height);
    }
    drop(recovered);
    if let Some(output) = std::env::var_os("MINTER_HIGH_ROUND_EXPORT") {
        let output = std::path::PathBuf::from(output);
        fs::create_dir(&output)?;
        fs::copy(root.join("genesis.json"), output.join("genesis.json"))?;
        fs::create_dir(output.join("state"))?;
        for entry in fs::read_dir(root.join("state"))? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file(),
                "unexpected nested state directory"
            );
            fs::copy(entry.path(), output.join("state").join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[test]
fn a_future_nil_quorum_keeps_the_proposer_priorities_in_sync() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    let mut expected = reactor.lock()?.validators.clone();
    proposer::increment(&mut expected, 101)?;
    let peer = "1234567890123456789012345678901234567890".parse()?;
    for index in 0..3 {
        let vote = high_round_vote(&reactor, index, 101, Type::Precommit, None)?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
    }
    let round = reactor.lock()?;
    assert_eq!(round.number, 101);
    assert_eq!(
        proposer::to_wire(&round.proposer_validators),
        proposer::to_wire(&expected)
    );
    Ok(())
}

#[test]
fn unexpected_rounds_are_bounded_and_released_after_the_last_session() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    let peer = "1234567890123456789012345678901234567890".parse()?;
    reactor.peer_connected(peer)?;
    reactor.peer_connected(peer)?;
    let mut invalid = high_round_vote(&reactor, 0, 100, Type::Precommit, None)?;
    invalid.signature[0] ^= 1;
    assert!(
        reactor
            .receive(
                peer,
                Sum::Vote(cs::Vote {
                    vote: Some(invalid)
                })
            )
            .is_err()
    );
    assert!(reactor.lock()?.peers[&peer].rounds.is_empty());
    for number in [100, 200, 300] {
        let vote = high_round_vote(&reactor, 0, number, Type::Precommit, None)?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
    }
    assert!(reactor.lock()?.votes.contains_key(&100));
    assert!(reactor.lock()?.votes.contains_key(&200));
    assert!(!reactor.lock()?.votes.contains_key(&300));
    reactor.peer_disconnected(peer);
    assert!(reactor.lock()?.votes.contains_key(&100));
    reactor.peer_disconnected(peer);
    assert!(reactor.lock()?.peers.is_empty());
    assert!(!reactor.lock()?.votes.contains_key(&100));
    assert!(!reactor.lock()?.votes.contains_key(&200));
    Ok(())
}

#[test]
fn old_unvisited_rounds_do_not_accumulate_after_a_jump() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    reactor.lock()?.enter_propose(100, Instant::now())?;
    let peer = "1234567890123456789012345678901234567890".parse()?;
    for number in 2..25 {
        reactor.peer_connected(peer)?;
        let vote = high_round_vote(&reactor, 0, number, Type::Prevote, None)?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
        reactor.peer_disconnected(peer);
        assert_eq!(reactor.lock()?.votes.len(), 4);
    }
    Ok(())
}

#[test]
fn a_second_peer_keeps_its_contributed_catchup_round() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    let peers = [
        "1234567890123456789012345678901234567890".parse()?,
        "2234567890123456789012345678901234567890".parse()?,
    ];
    for (index, peer) in peers.into_iter().enumerate() {
        reactor.peer_connected(peer)?;
        let vote = high_round_vote(&reactor, index, 100, Type::Prevote, None)?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
    }
    reactor.peer_disconnected(peers[0]);
    assert_eq!(reactor.lock()?.votes[&100].prevotes.by_index.len(), 2);
    reactor.peer_disconnected(peers[1]);
    assert!(!reactor.lock()?.votes.contains_key(&100));
    Ok(())
}

#[test]
fn reconnecting_a_peer_does_not_reopen_the_distinct_vote_claim_quota() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    let peer = "1234567890123456789012345678901234567890".parse()?;
    for seed in 1..=(MAX_VOTE_CLAIMS + 5) as u8 {
        reactor.peer_connected(peer)?;
        let id = block::Id {
            hash: tendermint::Hash::Sha256([seed; 32]),
            part_set_header: block::parts::Header::new(1, tendermint::Hash::Sha256([seed; 32]))?,
        };
        reactor.receive(peer, majority_message(101, 0, Type::Precommit, Some(id)))?;
        let vote = high_round_vote(&reactor, 0, 0, Type::Precommit, Some(id))?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
        reactor.peer_disconnected(peer);
    }
    let round = reactor.lock()?;
    assert!(round.votes[&0].precommits.power.len() <= MAX_VOTE_CLAIMS + 2);
    Ok(())
}

#[test]
fn a_commit_certificate_survives_the_sending_peers_disconnect() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reactor = high_round_reactor(temp.path())?;
    let id = block::Id {
        hash: tendermint::Hash::Sha256([3; 32]),
        part_set_header: block::parts::Header::new(1, tendermint::Hash::Sha256([4; 32]))?,
    };
    let peer = "1234567890123456789012345678901234567890".parse()?;
    reactor.peer_connected(peer)?;
    for index in 0..3 {
        let vote = high_round_vote(&reactor, index, 100, Type::Precommit, Some(id))?;
        reactor.receive(peer, Sum::Vote(cs::Vote { vote: Some(vote) }))?;
    }
    reactor.peer_disconnected(peer);
    assert_eq!(reactor.lock()?.certified_block(), Some((100, id)));
    Ok(())
}

#[test]
fn late_precommit_extends_the_saved_commit_without_changing_execution() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let genesis = temp.path().join("genesis.json");
    fs::write(
        &genesis,
        include_bytes!("../../../tests/fixtures/native-four-validator-genesis.json"),
    )?;
    let db = temp.path().join("chain");
    let node = NativeNode::new(NativeSync::open(&db, 1, Some(&genesis), None)?);
    let preview = NativeConsensus::new(node.clone(), None)?;
    let proposer = preview.lock()?.selected_proposer(0)?;
    drop(preview);
    let keys: Vec<SigningKey> = ["majority", "minority", "third", "fourth"]
        .into_iter()
        .map(|role| {
            SigningKey::from(<[u8; 32]>::from(Sha256::digest(format!(
                "public Minter evidence fixture {role}"
            ))))
        })
        .collect();
    let signing_key = keys
        .iter()
        .find(|key| account::Id::from(PublicKey::from(key.verification_key())) == proposer)
        .context("fixture proposer key")?
        .clone();
    let public = PublicKey::from(signing_key.verification_key());
    let key_file = temp.path().join("test-key.json");
    fs::write(
        &key_file,
        serde_json::to_vec(&PrivValidatorKey {
            address: proposer,
            pub_key: public,
            priv_key: PrivateKey::from(signing_key),
        })?,
    )?;
    let chain = node.lock()?.chain_id().clone();
    let go_state = temp.path().join("go-state.json");
    fs::write(&go_state, br#"{"height":"0","round":0,"step":0}"#)?;
    let journal = temp.path().join("signer");
    ValidatorSigner::import(&journal, &key_file, &go_state, chain.clone())?;
    let reactor = NativeConsensus::new(
        node,
        Some(ValidatorSigner::open(&journal, &key_file, &chain)?),
    )?;
    let (id, validators) = {
        let mut round = reactor.lock()?;
        let mut signer = reactor.signer.lock().unwrap();
        let signer = signer.as_mut().context("signer")?;
        let index = round.local_validator(signer).context("local validator")?;
        round.enter_propose(0, Instant::now())?;
        reactor.advance(&mut round, signer, index)?;
        (
            round.block_for_round(0).context("proposal block")?,
            round.validators.clone(),
        )
    };
    let vote = |index: usize| -> Result<types::Vote> {
        let validator = &validators.validators[index];
        let key = keys
            .iter()
            .find(|key| PublicKey::from(key.verification_key()) == validator.pub_key)
            .context("vote key")?;
        let domain = Vote {
            vote_type: Type::Precommit,
            height: 101u32.into(),
            round: Default::default(),
            block_id: Some(id),
            timestamp: Some(Time::now()),
            validator_address: validator.address,
            validator_index: index.try_into()?,
            signature: None,
            extension: Vec::new(),
            extension_signature: None,
        };
        let canonical: types::CanonicalVote =
            tendermint::vote::CanonicalVote::new(domain.clone(), chain.clone()).into();
        let mut wire: types::Vote = domain.into();
        wire.signature = key
            .sign(&canonical.encode_length_delimited_to_vec())
            .to_bytes()
            .to_vec();
        Ok(wire)
    };
    let local = validators
        .validators
        .iter()
        .position(|v| v.address == proposer)
        .context("local index")?;
    let source = "1234567890123456789012345678901234567890".parse()?;
    reactor.peer_connected(source)?;
    reactor.peer_connected(source)?;
    for index in (0..4).filter(|i| *i != local) {
        reactor.receive(
            source,
            Sum::Vote(cs::Vote {
                vote: Some(vote(index)?),
            }),
        )?;
    }
    let head = reactor.node.lock()?.head()?;
    assert_eq!(head.height, 101);
    assert_eq!(reactor.lock()?.peers[&source].connections, 2);
    reactor.peer_disconnected(source);
    assert_eq!(reactor.lock()?.peers[&source].connections, 1);
    let original = reactor
        .node
        .lock()?
        .app
        .store
        .native_block_at(101)?
        .context("block record")?;
    let block_response = super::super::BlockRecord::decode(original.as_slice())?.block_response;
    let late = vote(local)?;
    let mut invalid = late.clone();
    invalid.signature[0] ^= 1;
    assert!(
        reactor
            .receive(
                source,
                Sum::Vote(cs::Vote {
                    vote: Some(invalid)
                })
            )
            .is_err()
    );
    assert_eq!(
        reactor
            .node
            .lock()?
            .app
            .store
            .native_block_at(101)?
            .unwrap(),
        original
    );
    reactor.receive(
        source,
        Sum::Vote(cs::Vote {
            vote: Some(late.clone()),
        }),
    )?;
    assert!(
        reactor
            .lock()?
            .last_commit
            .as_ref()
            .unwrap()
            .signatures
            .iter()
            .all(|s| !s.is_absent())
    );
    assert_eq!(reactor.node.lock()?.head()?, head);
    let extended = reactor
        .node
        .lock()?
        .app
        .store
        .native_block_at(101)?
        .unwrap();
    reactor.receive(source, Sum::Vote(cs::Vote { vote: Some(late) }))?;
    assert_eq!(
        reactor
            .node
            .lock()?
            .app
            .store
            .native_block_at(101)?
            .unwrap(),
        extended
    );
    drop(reactor);
    let reopened = NativeConsensus::new(
        NativeNode::new(NativeSync::open(&db, 1, Some(&genesis), None)?),
        None,
    )?;
    assert!(
        reopened
            .lock()?
            .last_commit
            .as_ref()
            .unwrap()
            .signatures
            .iter()
            .all(|s| !s.is_absent())
    );
    let record = reopened
        .node
        .lock()?
        .app
        .store
        .native_block_at(101)?
        .context("reopened block")?;
    assert_eq!(
        super::super::BlockRecord::decode(record.as_slice())?.block_response,
        block_response
    );
    assert_eq!(reopened.node.lock()?.head()?, head);
    Ok(())
}

#[test]
fn conflicting_votes_keep_unique_power_and_recover_quorum_after_a_peer_claim() -> Result<()> {
    let id = |seed: u8| -> Result<Option<block::Id>> {
        Ok(Some(block::Id {
            hash: tendermint::Hash::Sha256([seed; 32]),
            part_set_header: block::parts::Header::new(1, tendermint::Hash::Sha256([seed; 32]))?,
        }))
    };
    let honest = id(1)?;
    let alternate = id(2)?;
    let vote = |index: usize, block_id| -> Result<Vote> {
        let key = SigningKey::from([index as u8 + 1; 32]);
        let public: PublicKey = key.verification_key().into();
        Ok(Vote {
            vote_type: Type::Precommit,
            height: 101u32.into(),
            round: Default::default(),
            block_id,
            timestamp: Some(Time::unix_epoch()),
            validator_address: account::Id::from(public),
            validator_index: index.try_into()?,
            signature: None,
            extension: Vec::new(),
            extension_signature: None,
        })
    };
    let mut votes = Votes::default();
    assert!(!votes.insert(0, vote(0, None)?, 2, 10));
    assert!(votes.insert(0, vote(0, alternate)?, 2, 10));
    assert!(!votes.insert(0, vote(0, honest)?, 2, 10));
    votes.insert(1, vote(1, honest)?, 3, 10);
    votes.insert(2, vote(2, honest)?, 3, 10);
    assert_eq!(votes.total_seen, 8);
    assert_eq!(votes.quorum, None);
    let peer = "1234567890123456789012345678901234567890".parse()?;
    votes.claim(peer, honest)?;
    assert!(votes.claim(peer, alternate).is_err());
    votes.insert(0, vote(0, honest)?, 2, 10);
    votes.insert(0, vote(0, honest)?, 2, 10);
    assert_eq!(votes.total_seen, 8);
    assert_eq!(votes.power[&honest], 8);
    assert_eq!(votes.quorum, Some(honest));
    assert_eq!(
        votes
            .canonical()
            .map(|(_, vote)| vote.block_id)
            .collect::<Vec<_>>(),
        vec![honest; 3]
    );
    assert_eq!(votes.second[&0].block_id, alternate);
    Ok(())
}

#[test]
fn restores_precommit_and_serves_older_commits_after_restart() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fixture: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/go-abci-v034.json"))?;
    let init = fixture["exchanges"]
        .as_array()
        .context("ABCI exchanges")?
        .iter()
        .find(|row| row["label"] == "init")
        .and_then(|row| row["request"].as_str())
        .context("ABCI InitChain")?;
    let Some(request::Value::InitChain(init)) =
        Request::decode(hex::decode(init)?.as_slice())?.value
    else {
        anyhow::bail!("expected InitChain");
    };
    let chain: chain::Id = init.chain_id.parse()?;
    let genesis = temp.path().join("genesis.json");
    fs::write(
        &genesis,
        serde_json::to_vec(&json!({
            "genesis_time": Time::try_from(init.time.context("missing genesis time")?)?,
            "chain_id": init.chain_id,
            "initial_height": init.initial_height.to_string(),
            "consensus_params": tendermint::consensus::Params::try_from(
                init.consensus_params.context("missing consensus parameters")?,
            )?,
            "app_hash": "",
            "app_state": serde_json::from_slice::<Value>(&init.app_state_bytes)?,
        }))?,
    )?;
    let signing_key = SigningKey::from(<[u8; 32]>::from(Sha256::digest(
        b"public Minter fixture validator",
    )));
    let public: PublicKey = signing_key.verification_key().into();
    let address = account::Id::from(public);
    let key = temp.path().join("test-key.json");
    fs::write(
        &key,
        serde_json::to_vec(&PrivValidatorKey {
            address,
            pub_key: public,
            priv_key: PrivateKey::from(signing_key),
        })?,
    )?;
    let go_state = temp.path().join("go-state.json");
    fs::write(&go_state, br#"{"height":"0","round":0,"step":0}"#)?;
    let journal = temp.path().join("signer");
    ValidatorSigner::import(&journal, &key, &go_state, chain.clone())?;
    let db = temp.path().join("chain");
    let open = || -> Result<NativeConsensus> {
        NativeConsensus::new(
            NativeNode::new(NativeSync::open(&db, 1, Some(&genesis), None)?),
            Some(ValidatorSigner::open(&journal, &key, &chain)?),
        )
    };

    let reactor = open()?;
    let vote = {
        let mut round = reactor.lock()?;
        let mut signer = reactor.signer.lock().unwrap();
        let signer = signer.as_mut().context("missing signer")?;
        let index = round.local_validator(signer).context("fixture validator")?;
        round.enter_propose(0, Instant::now())?;
        reactor.advance(&mut round, signer, index)?;
        assert_eq!(round.step, PREVOTE);
        reactor.advance(&mut round, signer, index)?;
        assert_eq!(round.step, PRECOMMIT);
        assert!(round.locked.is_some());
        signer.signed_vote(index.try_into()?)?
    };
    assert_eq!(reactor.node.lock()?.head()?.height, 100);
    drop(reactor);

    let recovered = open()?;
    {
        let round = recovered.lock()?;
        let id = round.locked.context("missing restored lock")?.1;
        assert_eq!(round.step, PRECOMMIT);
        let stored = round.votes[&0].precommits.by_index[&(vote.validator_index as usize)].clone();
        assert_eq!(types::Vote::from(stored), vote);
        assert_eq!(
            round
                .parts(id.part_set_header)
                .context("missing block parts")?
                .block()?
                .context("missing restored block")?
                .id(),
            id
        );
    }
    recovered.tick()?;
    assert_eq!(recovered.node.lock()?.head()?.height, 101);
    for height in 102..=104 {
        let mut round = recovered.lock()?;
        let mut signer = recovered.signer.lock().unwrap();
        let signer = signer.as_mut().context("missing signer")?;
        let index = round.local_validator(signer).context("fixture validator")?;
        round.enter_propose(0, Instant::now())?;
        recovered.advance(&mut round, signer, index)?;
        recovered.advance(&mut round, signer, index)?;
        recovered.publish_commit(&mut round, 0)?;
        assert_eq!(recovered.node.lock()?.head()?.height, height);
    }
    drop(recovered);
    let reopened = open()?;
    let mut gossip = Gossip::default();
    for height in [101, 103, 104, 101] {
        let messages = reopened.pending(
            &cs::NewRoundStep {
                height,
                round: 2,
                step: PROPOSE,
                ..Default::default()
            },
            &mut gossip,
        )?;
        assert!(
            messages.iter().any(|message| matches!(message,
                Sum::Vote(cs::Vote { vote: Some(vote) }) if vote.height == height && vote.round == 0
                    && vote.r#type == SignedMsgType::Precommit as i32 && !vote.signature.is_empty()
            )),
            "missing saved precommit for {height}"
        );
        assert!(
            messages.iter().any(|message| matches!(message,
                Sum::BlockPart(part) if part.height == height && part.round == 2
            )),
            "missing catch-up parts for {height}"
        );
    }
    let observer = NativeConsensus::new(
        NativeNode::new(NativeSync::open(
            &temp.path().join("observer"),
            1,
            Some(&genesis),
            None,
        )?),
        None,
    )?;
    let source = "1234567890123456789012345678901234567890".parse()?;
    for height in 101..=104 {
        let peer = observer.round_step()?;
        for message in reopened.pending(&peer, &mut gossip)? {
            observer.receive(source, message)?;
        }
        assert_eq!(observer.node.lock()?.head()?.height, height);
    }
    assert_eq!(observer.node.lock()?.head()?, reopened.node.lock()?.head()?);
    Ok(())
}
