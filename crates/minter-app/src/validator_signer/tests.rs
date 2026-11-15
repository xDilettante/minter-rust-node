use super::*;
use tendermint::private_key::PrivateKey;

fn fixture(root: &Path, seed: u8) -> (PathBuf, chain::Id) {
    let key = SigningKey::from([seed; 32]);
    let public_key: PublicKey = key.verification_key().into();
    let file = root.join(format!("key-{seed}.json"));
    std::fs::write(
        &file,
        serde_json::to_vec(&PrivValidatorKey {
            address: account::Id::from(public_key),
            pub_key: public_key,
            priv_key: PrivateKey::from(key),
        })
        .unwrap(),
    )
    .unwrap();
    (file, "minter-signer-test".parse().unwrap())
}

fn new_journal(root: &Path, key: &Path, chain: &chain::Id) -> PathBuf {
    let go = root.join("go-state.json");
    std::fs::write(&go, r#"{"height":"0","round":0,"step":0}"#).unwrap();
    let db = root.join("signer-db");
    ValidatorSigner::import(&db, key, &go, chain.clone()).unwrap();
    db
}

use std::path::PathBuf;

fn block_id(seed: u8) -> types::BlockId {
    types::BlockId {
        hash: vec![seed; 32],
        part_set_header: Some(types::PartSetHeader {
            total: 1,
            hash: vec![seed.wrapping_add(1); 32],
        }),
    }
}

fn time(nanos: i32) -> Option<Timestamp> {
    Some(Timestamp {
        seconds: 1_735_689_600,
        nanos,
    })
}

fn prevote(public: PublicKey, height: i64, round: i32) -> types::Vote {
    types::Vote {
        r#type: SignedMsgType::Prevote.into(),
        height,
        round,
        block_id: None,
        timestamp: time(42),
        validator_address: account::Id::from(public).as_bytes().to_vec(),
        validator_index: 0,
        signature: Vec::new(),
    }
}

#[test]
fn reopens_and_reuses_the_last_signature_for_only_a_timestamp_change() {
    let dir = tempfile::tempdir().unwrap();
    let (key, chain) = fixture(dir.path(), 7);
    let db = new_journal(dir.path(), &key, &chain);
    let mut signer = ValidatorSigner::open(&db, &key, &chain).unwrap();
    assert!(ValidatorSigner::open(&db, &key, &chain).is_err());
    let mut proposal = types::Proposal {
        r#type: SignedMsgType::Proposal.into(),
        height: 101,
        round: 1,
        pol_round: 0,
        block_id: Some(block_id(3)),
        timestamp: time(42),
        signature: Vec::new(),
    };
    signer.sign_proposal(&mut proposal).unwrap();
    drop(signer);

    let mut signer = ValidatorSigner::open(&db, &key, &chain).unwrap();
    let mut retry = proposal.clone();
    retry.timestamp = time(43);
    retry.signature.clear();
    signer.sign_proposal(&mut retry).unwrap();
    assert_eq!(proposal, retry);
    retry.pol_round = -1;
    assert!(matches!(
        signer.sign_proposal(&mut retry),
        Err(SignError::Conflict)
    ));
    let mut vote = prevote(signer.public_key(), 101, 1);
    signer.sign_vote(&mut vote).unwrap();
    assert!(matches!(
        signer.sign_proposal(&mut proposal),
        Err(SignError::Regression)
    ));
    vote.block_id = Some(block_id(3));
    assert!(matches!(
        signer.sign_vote(&mut vote),
        Err(SignError::Conflict)
    ));
    vote.r#type = SignedMsgType::Precommit.into();
    signer.sign_vote(&mut vote).unwrap();
    drop(signer);

    let mut signer = ValidatorSigner::open(&db, &key, &chain).unwrap();
    let mut retry = vote.clone();
    retry.timestamp = time(73);
    retry.signature.clear();
    signer.sign_vote(&mut retry).unwrap();
    assert_eq!(vote, retry);
    for (height, round) in [(100, 9), (101, 0), (101, 1)] {
        let mut earlier = prevote(signer.public_key(), height, round);
        assert!(matches!(
            signer.sign_vote(&mut earlier),
            Err(SignError::Regression)
        ));
    }
    let mut next = prevote(signer.public_key(), 101, 2);
    next.validator_address[0] ^= 1;
    assert!(matches!(
        signer.sign_vote(&mut next),
        Err(SignError::Invalid(_))
    ));
    let mut next = prevote(signer.public_key(), 101, 2);
    signer.sign_vote(&mut next).unwrap();
}

#[test]
fn imports_signed_state_only_for_its_chain_key_and_position() {
    let dir = tempfile::tempdir().unwrap();
    let (key, chain) = fixture(dir.path(), 8);
    let (other_key, _) = fixture(dir.path(), 9);
    let other_chain = "other-chain".parse().unwrap();
    let db = new_journal(dir.path(), &key, &chain);
    let mut signer = ValidatorSigner::open(&db, &key, &chain).unwrap();
    let mut original = prevote(signer.public_key(), 102, 0);
    signer.sign_vote(&mut original).unwrap();
    let saved = serde_json::to_vec(&signer.state.last).unwrap();
    drop(signer);
    assert!(ValidatorSigner::open(&db, &other_key, &chain).is_err());
    assert!(ValidatorSigner::open(&db, &key, &other_chain).is_err());
    assert!(ValidatorSigner::open(&dir.path().join("missing"), &key, &chain).is_err());

    let state_file = dir.path().join("signed-go-state.json");
    let imported = dir.path().join("imported");
    std::fs::write(&state_file, &saved).unwrap();
    assert!(ValidatorSigner::import(&db, &key, &state_file, chain.clone()).is_err());
    assert!(ValidatorSigner::import(&imported, &key, &state_file, other_chain).is_err());
    assert!(ValidatorSigner::import(&imported, &other_key, &state_file, chain.clone()).is_err());
    let mut last: LastSign = serde_json::from_slice(&saved).unwrap();
    last.height += 1;
    std::fs::write(&state_file, serde_json::to_vec(&last).unwrap()).unwrap();
    assert!(ValidatorSigner::import(&imported, &key, &state_file, chain.clone()).is_err());
    std::fs::write(&state_file, &saved).unwrap();
    ValidatorSigner::import(&imported, &key, &state_file, chain.clone()).unwrap();
    let mut signer = ValidatorSigner::open(&imported, &key, &chain).unwrap();
    let mut retry = prevote(signer.public_key(), 102, 0);
    retry.timestamp = time(101);
    signer.sign_vote(&mut retry).unwrap();
    assert_eq!(original, retry);
}
