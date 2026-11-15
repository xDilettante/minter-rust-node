use super::*;
use ed25519_consensus::SigningKey;
use sha2::Digest;
use tendermint::{
    PublicKey, account, block, validator,
    vote::{Type, Vote},
};
use tendermint_proto::v0_34::version;

fn fixture() -> Result<(Record, Header, Set)> {
    fixture_with_evidence(types::EvidenceList::default())
}

fn fixture_with_evidence(evidence: types::EvidenceList) -> Result<(Record, Header, Set)> {
    let keys: Vec<_> = (1..=4).map(|i| SigningKey::from([i; 32])).collect();
    let set = crate::native_sync::validator_set(
        keys.iter()
            .map(|key| validator::Info::new(PublicKey::from(key.verification_key()), 1u32.into()))
            .collect(),
    )?;
    let empty = Hasher::digest([]).to_vec();
    let header: Header = types::Header {
        version: Some(version::Consensus { block: 11, app: 0 }),
        chain_id: "history-context-test".into(),
        height: 10,
        time: Some(Time::unix_epoch().into()),
        last_block_id: Some(block::Id::default().into()),
        last_commit_hash: empty.clone(),
        data_hash: empty.clone(),
        validators_hash: set.hash().as_bytes().to_vec(),
        next_validators_hash: set.hash().as_bytes().to_vec(),
        consensus_hash: empty.clone(),
        app_hash: vec![1; 32],
        last_results_hash: empty.clone(),
        evidence_hash: tendermint::merkle::simple_hash_from_byte_vectors::<Hasher>(
            &evidence
                .evidence
                .iter()
                .map(wire::payload)
                .collect::<Result<Vec<_>>>()?,
        )
        .to_vec(),
        proposer_address: set.validators[0].address.as_bytes().to_vec(),
    }
    .try_into()?;
    let id = block::Id {
        hash: header.hash(),
        part_set_header: block::parts::Header::new(1, Hash::Sha256([2; 32]))?,
    };
    let mut signatures = Vec::new();
    for (index, validator) in set.validators.iter().enumerate() {
        let key = keys
            .iter()
            .find(|key| {
                account::Id::from(PublicKey::from(key.verification_key())) == validator.address
            })
            .context("fixture key")?;
        let vote = Vote {
            vote_type: Type::Precommit,
            height: header.height,
            round: Default::default(),
            block_id: Some(id),
            timestamp: Some(header.time),
            validator_address: validator.address,
            validator_index: index.try_into()?,
            signature: None,
            extension: Vec::new(),
            extension_signature: None,
        };
        let canonical: types::CanonicalVote =
            tendermint::vote::CanonicalVote::new(vote, header.chain_id.clone()).into();
        signatures.push(tendermint::block::CommitSig::BlockIdFlagCommit {
            validator_address: validator.address,
            timestamp: header.time,
            signature: Some(
                key.sign(&canonical.encode_length_delimited_to_vec())
                    .to_bytes()
                    .to_vec()
                    .try_into()?,
            ),
        });
    }
    let commit = Commit {
        height: header.height,
        round: Default::default(),
        block_id: id,
        signatures,
    };
    let mut successor = header.clone();
    successor.height = 11u32.into();
    successor.last_block_id = Some(id);
    successor.last_commit_hash = Some(verify::commit_hash(Some(&commit)));
    let record = Record {
        signed_header: types::SignedHeader {
            header: Some(header.into()),
            commit: Some(commit.into()),
        }
        .encode_to_vec(),
        validators: membership(&set).encode_to_vec(),
        evidence: evidence.encode_to_vec(),
    };
    Ok((record, successor, set))
}

#[test]
fn a_quorum_alone_does_not_replace_the_chain_selected_commit() -> Result<()> {
    let (mut record, successor, set) = fixture()?;
    let chain = successor.chain_id.clone();
    assert_eq!(validate(&record, &successor, &chain)?.0.height.value(), 10);
    let (header, mut commit) = decode_signed(&record.signed_header)?;
    commit.signatures[0] = tendermint::block::CommitSig::BlockIdFlagAbsent;
    // Three of four still authenticate the block, but this is not the selected list.
    verify::verify_commit(&chain, header.height, commit.block_id, &set, &commit)?;
    record.signed_header = types::SignedHeader {
        header: Some(header.into()),
        commit: Some(commit.into()),
    }
    .encode_to_vec();
    assert!(
        validate(&record, &successor, &chain)
            .unwrap_err()
            .to_string()
            .contains("LastCommitHash")
    );
    Ok(())
}

#[test]
fn rejects_wrong_membership_link_chain_proof_and_signature() -> Result<()> {
    let (record, successor, _) = fixture()?;
    let chain = successor.chain_id.clone();
    let mut changed = successor.clone();
    changed.last_block_id.as_mut().unwrap().hash = Hash::Sha256([9; 32]);
    assert!(validate(&record, &changed, &chain).is_err());
    assert!(validate(&record, &successor, &"other-chain".parse()?).is_err());
    let mut bad = record.clone();
    let mut set = types::ValidatorSet::decode(bad.validators.as_slice())?;
    set.validators[0].voting_power += 1;
    set.total_voting_power += 1;
    bad.validators = set.encode_to_vec();
    assert!(validate(&bad, &successor, &chain).is_err());
    let mut bad = record.clone();
    bad.evidence = vec![10, 0];
    assert!(validate(&bad, &successor, &chain).is_err());
    let mut bad = record;
    let mut signed = types::SignedHeader::decode(bad.signed_header.as_slice())?;
    signed.commit.as_mut().unwrap().signatures[0].signature[0] ^= 1;
    bad.signed_header = signed.encode_to_vec();
    assert!(validate(&bad, &successor, &chain).is_err());
    Ok(())
}

#[test]
fn frame_limits_detect_oversize_overflow_and_truncation() -> Result<()> {
    let (record, _, _) = fixture()?;
    let mut bytes = Vec::new();
    write_message(&mut bytes, &record, MAX_RECORD)?;
    let mut input = bytes.as_slice();
    assert_eq!(
        read_message(&mut input, MAX_RECORD)?,
        record.encode_to_vec()
    );
    assert!(read_message(&mut input, MAX_RECORD).is_err());
    assert!(read_message(&mut bytes.as_slice(), 1).is_err());
    assert!(read_message(&mut [255u8; 10].as_slice(), MAX_RECORD).is_err());
    bytes.pop();
    assert!(read_message(&mut bytes.as_slice(), MAX_RECORD).is_err());
    Ok(())
}

#[test]
fn included_proof_hashes_require_the_signed_root_and_unique_entries() -> Result<()> {
    let (_, successor, set) = fixture()?;
    let key = SigningKey::from([1; 32]);
    let address = account::Id::from(PublicKey::from(key.verification_key()));
    let index = set
        .validators
        .iter()
        .position(|v| v.address == address)
        .context("fixture index")?;
    let vote = |seed| -> Result<Vote> {
        let mut vote = Vote {
            vote_type: Type::Prevote,
            height: 9u32.into(),
            round: Default::default(),
            block_id: Some(block::Id {
                hash: Hash::Sha256([seed; 32]),
                part_set_header: block::parts::Header::new(1, Hash::Sha256([seed; 32]))?,
            }),
            timestamp: Some(Time::unix_epoch()),
            validator_address: address,
            validator_index: index.try_into()?,
            signature: None,
            extension: Vec::new(),
            extension_signature: None,
        };
        let canonical: types::CanonicalVote =
            tendermint::vote::CanonicalVote::new(vote.clone(), successor.chain_id.clone()).into();
        vote.signature = Some(
            key.sign(&canonical.encode_length_delimited_to_vec())
                .to_bytes()
                .to_vec()
                .try_into()?,
        );
        Ok(vote)
    };
    let proof = wire::duplicate_vote(&vote(3)?, &vote(4)?, Time::unix_epoch(), &set)?;
    let hash = wire::hash(&proof)?;
    let (mut record, successor, _) = fixture_with_evidence(types::EvidenceList {
        evidence: vec![proof.clone()],
    })?;
    assert_eq!(
        validate(&record, &successor, &successor.chain_id)?.2,
        vec![hash]
    );
    record.evidence.clear();
    assert!(
        validate(&record, &successor, &successor.chain_id)
            .unwrap_err()
            .to_string()
            .contains("EvidenceHash")
    );
    let (record, successor, _) = fixture_with_evidence(types::EvidenceList {
        evidence: vec![proof.clone(), proof],
    })?;
    assert!(
        validate(&record, &successor, &successor.chain_id)
            .unwrap_err()
            .to_string()
            .contains("repeated historical evidence")
    );
    Ok(())
}
