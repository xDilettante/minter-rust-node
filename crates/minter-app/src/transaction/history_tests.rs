//! Historical wire/signature/gas checks; these do not replay mainnet state.

use super::{Authorization, Data, Envelope, total_gas};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    execution_verified: bool,
    trusted_chain_path_verified: bool,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    case: String,
    height: u64,
    raw_transaction: String,
    transaction_hash: String,
    sender: String,
    nonce: u64,
    chain_id: u8,
    gas_coin: u32,
    gas_price: u32,
    gas: i64,
    kind: u8,
    name: String,
    symbol: String,
    initial_amount: String,
    max_supply: String,
    mintable: bool,
    burnable: bool,
}

#[test]
fn real_mainnet_recreation_wire_signatures_payload_and_gas_match_go() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../tests/fixtures/mainnet-recreation-history.json"
    ))
    .unwrap();
    assert!(!corpus.execution_verified && !corpus.trusted_chain_path_verified);
    let identities: Vec<_> = corpus
        .cases
        .iter()
        .map(|case| {
            (
                case.case.as_str(),
                case.height,
                case.transaction_hash.as_str(),
            )
        })
        .collect();
    assert_eq!(
        identities,
        [
            (
                "legal",
                15_190_549,
                "b41e54a6e63210247fced2f6553723389526a3ea006746f391b6ab07bc1cf0a7"
            ),
            (
                "interstake",
                20_220_176,
                "766b42b8cb18efa9b50f0b60a7ee0b77a8ec2a3a293cb38b00a8ed6f6d5ff529"
            ),
            (
                "bazercash",
                23_497_951,
                "ad4549948ad553cd5fe4f0fe57957618c16740c8cb54b832830736ce09dd730d"
            ),
        ]
    );
    for case in corpus.cases {
        let raw = hex::decode(case.raw_transaction).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&raw)), case.transaction_hash);
        let envelope = Envelope::decode(&raw).unwrap();
        let fields = envelope.fields();
        assert_eq!(fields.chain_id, case.chain_id);
        assert_eq!(fields.nonce, case.nonce);
        assert_eq!(fields.gas_coin, case.gas_coin);
        assert_eq!(fields.gas_price, case.gas_price);
        assert_eq!(fields.kind, case.kind);
        let Authorization::Single(signature) = envelope.authorization() else {
            panic!("expected a single signature");
        };
        assert_eq!(
            format!(
                "Mx{}",
                hex::encode(signature.recover(envelope.signing_hash()).unwrap())
            ),
            case.sender
        );
        let data = Data::decode(fields.kind, fields.data).unwrap();
        assert_eq!(data.type_tag(), "1f");
        assert_eq!(total_gas(fields, fields.gas_coin, data.gas(), 0), case.gas);
        let Data::RecreateToken(creation) = data else {
            panic!("expected RecreateToken");
        };
        assert_eq!(creation.coin.name, case.name.as_bytes());
        assert_eq!(creation.coin.symbol.name(), case.symbol);
        assert_eq!(creation.supply.volume.to_string(), case.initial_amount);
        assert_eq!(creation.coin.max_supply.to_string(), case.max_supply);
        assert_eq!(creation.coin.mintable, case.mintable);
        assert_eq!(creation.coin.burnable, case.burnable);
        assert_eq!(creation.coin.ratio, 0);
        assert_eq!(creation.supply.reserve.bits(), 0);
    }
}
