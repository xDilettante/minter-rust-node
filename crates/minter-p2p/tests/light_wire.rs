use anyhow::{Context, Result};
use minter_p2p::{ReceivedBlock, consensus::BlockParts, evidence};
use prost::Message;
use serde_json::Value;
use tendermint::{block::signed_header::SignedHeader, validator::Set};
use tendermint_proto::v0_34::types;

#[test]
fn matches_go_light_wire_verification_and_block_decoding() -> Result<()> {
    let reference: Value = serde_json::from_str(include_str!("fixtures/go-light-wire.json"))?;
    let bytes = |value: &Value| -> Result<Vec<u8>> {
        Ok(hex::decode(value.as_str().context("hex fixture")?)?)
    };
    let common: Set =
        types::ValidatorSet::decode(bytes(&reference["commonValidators"])?.as_slice())?
            .try_into()?;
    let trusted: SignedHeader =
        types::SignedHeader::decode(bytes(&reference["trustedHeader"])?.as_slice())?.try_into()?;
    let mut mismatches = Vec::new();
    for case in reference["cases"].as_array().context("fixture cases")? {
        let name = case["name"].as_str().context("case name")?;
        let item = types::Evidence::decode(bytes(&case["proof"])?.as_slice())?;
        let accepted = (|| -> Result<()> {
            let Some(types::evidence::Sum::LightClientAttackEvidence(proof)) = &item.sum else {
                anyhow::bail!("not a light proof");
            };
            evidence::verify_light_attack(proof, &common, &trusted.header, &trusted.commit)
        })();
        let block = ReceivedBlock::decode(bytes(&case["block"])?);
        if let Some(expected) = case["evidenceHash"].as_str() {
            assert_eq!(
                evidence::input_size(&types::EvidenceList {
                    evidence: vec![item.clone()]
                }),
                case["evidenceSize"].as_u64().context("Go input size")? as usize,
                "{name}: input protobuf size"
            );
            assert_eq!(
                hex::encode(evidence::hash(&item)?),
                expected,
                "{name}: evidence identity"
            );
        }
        if let Ok(block) = &block {
            assert_eq!(
                hex::encode(block.proto_header().encode_to_vec()),
                case["canonicalHeader"]
                    .as_str()
                    .context("Go header protobuf")?,
                "{name}: ABCI header bytes"
            );
            assert_eq!(
                block.evidence_size(),
                case["evidenceSize"].as_u64().context("Go evidence size")? as usize,
                "{name}: evidence size limit"
            );
            if let Some(expected) = case["blockHash"].as_str() {
                assert_eq!(
                    hex::encode(block.id().hash.as_bytes()),
                    expected,
                    "{name}: block identity"
                );
                assert_eq!(
                    hex::encode(block.id().part_set_header.hash.as_bytes()),
                    case["partSetHash"].as_str().context("Go PartSetHash")?,
                    "{name}: PartSet identity"
                );
            }
        }
        if accepted.is_ok() != case["accepted"].as_bool().context("Go verification")? {
            mismatches.push(format!(
                "{name}: verify Rust={accepted:?}, Go={} ({})",
                case["accepted"], case["reason"]
            ));
        }
        if block.is_ok()
            != case["blockDecoded"]
                .as_bool()
                .context("Go block decoding")?
        {
            mismatches.push(format!(
                "{name}: block Rust={}, Go={}",
                block.is_ok(),
                case["blockDecoded"]
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    Ok(())
}

#[test]
fn reconstructs_go_part_sets_from_all_decodable_wire_blocks() -> Result<()> {
    let reference: Value = serde_json::from_str(include_str!("fixtures/go-light-wire.json"))?;
    let mut failures = Vec::new();
    for case in reference["cases"].as_array().context("fixture cases")? {
        if case["blockDecoded"] != true {
            continue;
        }
        let name = case["name"].as_str().context("case name")?;
        let wire = hex::decode(case["block"].as_str().context("block bytes")?)?;
        let block = ReceivedBlock::decode(wire.clone())?;
        assert_eq!(block.wire(), wire, "{name}: preserved capture bytes");
        let id = block.id();
        let size = block.encoded_len();
        assert_eq!(
            size,
            case["canonicalSize"].as_u64().context("Go block size")? as usize,
            "{name}: canonical block size"
        );
        match BlockParts::from_block(block) {
            Ok(parts) => {
                assert_eq!(
                    parts.header().total as u64,
                    case["partCount"].as_u64().context("Go part count")?,
                    "{name}: part count"
                );
                assert_eq!(
                    hex::encode(parts.header().hash.as_bytes()),
                    case["partSetHash"].as_str().context("Go PartSetHash")?,
                    "{name}: generated parts"
                );
                let mut receiver = BlockParts::new(parts.header(), size)?;
                for index in (0..parts.header().total as usize).rev() {
                    receiver.insert(parts.part(index).context("part")?.clone())?;
                }
                assert_eq!(
                    receiver.block()?.context("rebuilt block")?.id(),
                    id,
                    "{name}"
                );
            }
            Err(error) => failures.push(format!("{name}: {error:#}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
