use anyhow::{Result, ensure};
use prost::encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field};

// Tendermint 0.34 types.MaxVotesCount. Inspect counts before prost allocates them.
const MAX_VALIDATORS: usize = 10_000;

pub(super) fn messages(
    mut input: &[u8],
    wanted: u32,
    limit: usize,
    mut visit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut count = 0;
    while !input.is_empty() {
        let (tag, kind) = decode_key(&mut input)?;
        if tag != wanted {
            skip_field(kind, tag, &mut input, DecodeContext::default())?;
            continue;
        }
        ensure!(
            kind == WireType::LengthDelimited,
            "invalid context protobuf field"
        );
        count += 1;
        ensure!(count <= limit, "too many context protobuf entries");
        let len: usize = decode_varint(&mut input)?.try_into()?;
        ensure!(len <= input.len(), "truncated context protobuf field");
        let (value, rest) = input.split_at(len);
        input = rest;
        visit(value)?;
    }
    Ok(())
}

pub(super) fn signed_header(input: &[u8]) -> Result<()> {
    ensure!(
        input.len() <= super::MAX_SET,
        "oversized historical signed header"
    );
    messages(input, 1, 1, |_| Ok(()))?;
    messages(input, 2, 1, |commit| {
        messages(commit, 4, MAX_VALIDATORS, |_| Ok(()))
    })
}

pub(super) fn validators(input: &[u8]) -> Result<()> {
    ensure!(
        input.len() <= super::MAX_SET,
        "oversized historical validator set"
    );
    messages(input, 1, MAX_VALIDATORS, |_| Ok(()))
}

pub(super) fn proof(input: &[u8]) -> Result<()> {
    messages(input, 1, 1, |_| Ok(()))?;
    messages(input, 2, 1, |proof| {
        messages(proof, 3, MAX_VALIDATORS, |_| Ok(()))?;
        messages(proof, 1, 1, |light| {
            messages(light, 1, 1, signed_header)?;
            messages(light, 2, 1, validators)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_repeated_empty_submessages_before_decoding() {
        let excessive = [10, 0].repeat(MAX_VALIDATORS + 1);
        assert!(validators(&excessive).is_err());
        let mut commit = [34, 0].repeat(MAX_VALIDATORS + 1);
        let mut outer = vec![18];
        prost::encoding::encode_varint(commit.len() as u64, &mut outer);
        outer.append(&mut commit);
        assert!(signed_header(&outer).is_err());
        assert!(messages(&[10, 100, 0], 1, 1, |_| Ok(())).is_err());
    }

    fn wrap(tag: u32, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        prost::encoding::encode_key(tag, WireType::LengthDelimited, &mut out);
        prost::encoding::encode_varint(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn bounds_nested_light_proof_vectors_and_bytes() {
        let validators = [10, 0].repeat(MAX_VALIDATORS + 1);
        let signatures = [34, 0].repeat(MAX_VALIDATORS + 1);
        let byzantine = [26, 0].repeat(MAX_VALIDATORS + 1);
        for bad in [
            wrap(2, &byzantine),
            wrap(2, &wrap(1, &wrap(2, &validators))),
            wrap(2, &wrap(1, &wrap(1, &wrap(2, &signatures)))),
        ] {
            assert!(proof(&bad).is_err());
        }
        let padding = wrap(15, &vec![0; super::super::MAX_SET]);
        for field in [1, 2] {
            let bad = wrap(2, &wrap(1, &wrap(field, &padding)));
            assert!(proof(&bad).unwrap_err().to_string().contains("oversized"));
        }
    }
}
