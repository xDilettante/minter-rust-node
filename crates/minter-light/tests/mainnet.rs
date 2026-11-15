use serde::Deserialize;
use std::time::Duration;
use tendermint::trust_threshold::TrustThresholdFraction;
use tendermint_light_client::{
    light_client::Options,
    types::LightBlock,
    verifier::{ProdVerifier, Verdict, Verifier},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Proofs {
    height: u64,
    app_hash: String,
    values: Vec<Query>,
}

#[derive(Deserialize)]
struct Query {
    key: String,
    value: Option<String>,
    proof: String,
}

#[test]
fn checks_mainnet_signatures_validator_transition_and_go_iavl_proofs() {
    let blocks: Vec<LightBlock> =
        serde_json::from_str(include_str!("fixtures/go-light-blocks.json")).unwrap();
    let verifier = ProdVerifier::default();
    let options = Options {
        trust_threshold: TrustThresholdFraction::ONE_THIRD,
        trusting_period: Duration::from_secs(7 * 86_400),
        clock_drift: Duration::from_secs(5),
    };
    let now = blocks
        .last()
        .unwrap()
        .time()
        .checked_add(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        blocks[0].signed_header.header.hash().to_string(),
        "9CFD40F71D4DB60FD6251F88BF7A39088332FACA8A53E3C41F8D7BEF0C1E255A"
    );
    for block in &blocks {
        assert_eq!(
            block.signed_header.header.chain_id.as_str(),
            "minter-mainnet-5"
        );
        assert_eq!(
            verifier.verify_validator_sets(&block.as_untrusted_state()),
            Verdict::Success
        );
        assert_eq!(
            verifier.verify_commit(&block.as_untrusted_state()),
            Verdict::Success
        );
    }
    for pair in blocks.windows(2) {
        assert_eq!(
            verifier.verify_update_header(
                pair[1].as_untrusted_state(),
                pair[0].as_trusted_state(),
                &options,
                now,
            ),
            Verdict::Success
        );
    }
    let header_at = |height| {
        &blocks
            .iter()
            .find(|block| block.height().value() == height)
            .unwrap()
            .signed_header
            .header
    };
    let transition = header_at(40_582_503);
    assert_ne!(transition.validators_hash, transition.next_validators_hash);
    assert_eq!(
        transition.next_validators_hash,
        header_at(40_582_504).validators_hash
    );

    let proofs: Proofs =
        serde_json::from_str(include_str!("fixtures/go-iavl-mainnet.json")).unwrap();
    let header = &blocks[0].signed_header.header;
    assert_eq!(proofs.height + 1, header.height.value());
    let root = header.app_hash.as_bytes().try_into().unwrap();
    assert_eq!(proofs.app_hash, hex::encode(root));
    for query in proofs.values {
        let result = minter_iavl::verify_proof(
            root,
            proofs.height,
            &hex::decode(query.key).unwrap(),
            &hex::decode(query.proof).unwrap(),
        )
        .unwrap();
        assert_eq!(result.map(hex::encode), query.value);
    }
}
