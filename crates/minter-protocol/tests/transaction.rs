mod support;

use alloy_rlp::{Decodable, Encodable, Header};
use minter_protocol::{Authorization, DecodeError, Envelope, InvalidSignature, MAX_TX_BYTES};

const GO_FIXTURE: &str = include_str!("fixtures/go-transactions.jsonl");

fn first_transaction() -> Vec<u8> {
    let first: serde_json::Value =
        serde_json::from_str(GO_FIXTURE.lines().next().unwrap()).unwrap();
    hex::decode(first["raw"].as_str().unwrap()).unwrap()
}

#[test]
fn matches_go_envelopes_from_mainnet() {
    let counts = support::compare(GO_FIXTURE.as_bytes()).unwrap();
    assert_eq!((counts.single, counts.multisig), (2, 1));
    assert!(counts.multisig_signatures > 0);
}

#[test]
fn rejects_oversized_trailing_and_noncanonical_encodings() {
    let raw = first_transaction();
    assert!(matches!(
        Envelope::decode(&vec![0; MAX_TX_BYTES + 1]),
        Err(DecodeError::TooLarge)
    ));
    let mut trailing = raw.clone();
    trailing.push(0);
    assert!(matches!(
        Envelope::decode(&trailing),
        Err(DecodeError::TrailingBytes)
    ));

    let mut input = raw.as_slice();
    let mut body = Header::decode_bytes(&mut input, true).unwrap();
    u64::decode(&mut body).unwrap();
    let noncanonical = list(&[&[0], body]);
    assert!(matches!(
        Envelope::decode(&noncanonical),
        Err(DecodeError::Rlp(_))
    ));
}

#[test]
fn rejects_disallowed_recovery_id_and_high_s() {
    let raw = first_transaction();
    let mut input = raw.as_slice();
    let body = Header::decode_bytes(&mut input, true).unwrap();
    let mut remainder = body;
    for _ in 0..9 {
        Header::decode_bytes(&mut remainder, false).unwrap();
    }
    let signed = &body[..body.len() - remainder.len()];
    let high_s =
        hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140").unwrap();
    let mut encoded_high_s = Vec::new();
    high_s.as_slice().encode(&mut encoded_high_s);
    for (signature, expected) in [
        (list(&[&[29, 1, 1]]), InvalidSignature::RecoveryId),
        (list(&[&[27, 1], &encoded_high_s]), InvalidSignature::HighS),
    ] {
        let mut encoded = Vec::new();
        signature.as_slice().encode(&mut encoded);
        let raw = list(&[signed, &encoded]);
        let tx = Envelope::decode(&raw).unwrap();
        let Authorization::Single(signature) = tx.authorization() else {
            panic!("fixture must use a single signature");
        };
        assert_eq!(signature.recover(tx.signing_hash()), Err(expected));
    }
}

fn list(parts: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    Header {
        list: true,
        payload_length: parts.iter().map(|part| part.len()).sum(),
    }
    .encode(&mut bytes);
    for part in parts {
        bytes.extend_from_slice(part);
    }
    bytes
}
