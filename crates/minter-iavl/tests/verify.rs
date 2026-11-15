use minter_iavl::{
    Content, Hash, Node, empty_hash,
    ics23::{CommitmentProof, ExistenceProof, NonExistenceProof, commitment_proof::Proof},
    verify_proof,
};
use prost::Message;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    height: u64,
    #[serde(with = "hex::serde")]
    app_hash: Hash,
    values: Vec<Query>,
}

#[derive(Deserialize)]
struct Query {
    #[serde(with = "hex::serde")]
    key: Vec<u8>,
    value: Option<String>,
    #[serde(with = "hex::serde")]
    proof: Vec<u8>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/go-ics23.json")).unwrap()
}

#[test]
fn checks_go_proofs_with_missing_keys_and_empty_committed_values() {
    let fixture = fixture();
    for item in fixture.values {
        assert_eq!(
            verify_proof(fixture.app_hash, fixture.height, &item.key, &item.proof).unwrap(),
            item.value.map(|value| hex::decode(value).unwrap()),
            "key {}",
            hex::encode(item.key),
        );
    }
}

fn existence(fixture: &Fixture, key: &[u8]) -> ExistenceProof {
    let query = fixture.values.iter().find(|item| item.key == key).unwrap();
    let proof = CommitmentProof::decode(query.proof.as_slice()).unwrap();
    let Some(Proof::Exist(proof)) = proof.proof else {
        panic!("expected a Go membership proof");
    };
    proof
}

#[test]
fn rejects_wrong_commitments_and_fabricated_neighbors() {
    let fixture = fixture();
    let verify = |key: &[u8], proof: &CommitmentProof| {
        verify_proof(
            fixture.app_hash,
            fixture.height,
            key,
            &proof.encode_to_vec(),
        )
    };
    let key = b"a";
    let mut leaf = existence(&fixture, key);
    let correct = CommitmentProof {
        proof: Some(Proof::Exist(leaf.clone())),
    };
    assert!(verify(b"b", &correct).is_err());
    assert!(verify_proof([1; 32], fixture.height, key, &correct.encode_to_vec()).is_err());
    assert!(verify_proof(fixture.app_hash, 1, key, &correct.encode_to_vec()).is_err());
    leaf.value[0] ^= 1;
    assert!(
        verify(
            key,
            &CommitmentProof {
                proof: Some(Proof::Exist(leaf))
            }
        )
        .is_err()
    );

    let mut absence = NonExistenceProof {
        key: key.to_vec(),
        left: Some(existence(&fixture, b"\x00")),
        right: Some(existence(&fixture, b"m")),
    };
    assert!(
        verify(
            key,
            &CommitmentProof {
                proof: Some(Proof::Nonexist(absence.clone()))
            }
        )
        .is_err()
    );
    absence.right = None;
    assert!(
        verify(
            key,
            &CommitmentProof {
                proof: Some(Proof::Nonexist(absence))
            }
        )
        .is_err()
    );

    let mut correct_absence = CommitmentProof::decode(fixture.values[1].proof.as_slice()).unwrap();
    let Some(Proof::Nonexist(proof)) = correct_absence.proof.as_mut() else {
        panic!("expected a Go nonmembership proof");
    };
    proof.key = vec![2];
    assert!(verify(b"\x01", &correct_absence).is_err());
}

#[test]
fn accepts_empty_tree_absence_only_against_the_empty_commitment() {
    let proof = CommitmentProof {
        proof: Some(Proof::Nonexist(NonExistenceProof {
            key: b"a".to_vec(),
            ..Default::default()
        })),
    }
    .encode_to_vec();
    assert_eq!(verify_proof(empty_hash(), 1, b"a", &proof).unwrap(), None);
    assert!(verify_proof([0; 32], 1, b"a", &proof).is_err());
    assert!(verify_proof(empty_hash(), 1, b"b", &proof).is_err());
}

#[test]
fn checks_a_single_leaf_with_an_empty_key_and_value() {
    let node = Node {
        height: 0,
        size: 1,
        version: 1,
        key: &[],
        content: Content::Leaf(&[]),
    };
    let root = node.hash(&mut Vec::new());
    let leaf = node.existence_proof(Vec::new()).unwrap();
    let proof = CommitmentProof {
        proof: Some(Proof::Exist(leaf.clone())),
    }
    .encode_to_vec();
    assert_eq!(
        verify_proof(root, 1, b"", &proof).unwrap(),
        Some(Vec::new())
    );
    let proof = CommitmentProof {
        proof: Some(Proof::Nonexist(NonExistenceProof {
            key: b"a".to_vec(),
            left: Some(leaf),
            right: None,
        })),
    }
    .encode_to_vec();
    assert_eq!(verify_proof(root, 1, b"a", &proof).unwrap(), None);
}
