//! Wire validation follows Go 0.34.19 before using its light verification rules.
use super::*;
use k256::ecdsa::signature::hazmat::PrehashVerifier;
use std::{cmp::Reverse, collections::BTreeSet};
use tendermint_proto::{google::protobuf::Timestamp, v0_34::crypto::public_key::Sum as Key};

pub(super) const ZERO_TIME: Timestamp = Timestamp {
    seconds: -62_135_596_800,
    nanos: 0,
};

fn time(value: Option<Timestamp>) -> Result<Timestamp> {
    let value = value.unwrap_or(ZERO_TIME);
    let _: Time = value.try_into()?;
    Ok(value)
}

fn hash_size(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.is_empty() || bytes.len() == 32, "invalid hash length");
    Ok(())
}

fn normalize_id(raw: Option<types::BlockId>) -> types::BlockId {
    let mut id = raw.unwrap_or_default();
    id.part_set_header.get_or_insert_with(Default::default);
    id
}

fn basic_id(raw: Option<&types::BlockId>) -> Result<()> {
    if let Some(id) = raw {
        hash_size(&id.hash)?;
        if let Some(parts) = &id.part_set_header {
            hash_size(&parts.hash)?;
        }
    }
    Ok(())
}

pub(super) fn header_hash(header: &types::Header) -> Result<[u8; 32]> {
    ensure!(!header.validators_hash.is_empty(), "missing ValidatorsHash");
    let fields = vec![
        header.version.unwrap_or_default().encode_to_vec(),
        header.chain_id.encode_to_vec(),
        header.height.encode_to_vec(),
        time(header.time)?.encode_to_vec(),
        normalize_id(header.last_block_id.clone()).encode_to_vec(),
        header.last_commit_hash.encode_to_vec(),
        header.data_hash.encode_to_vec(),
        header.validators_hash.encode_to_vec(),
        header.next_validators_hash.encode_to_vec(),
        header.consensus_hash.encode_to_vec(),
        header.app_hash.encode_to_vec(),
        header.last_results_hash.encode_to_vec(),
        header.evidence_hash.encode_to_vec(),
        header.proposer_address.encode_to_vec(),
    ];
    Ok(tendermint::merkle::simple_hash_from_byte_vectors::<Hasher>(
        &fields,
    ))
}

fn basic_header(header: &types::Header) -> Result<()> {
    ensure!(
        header
            .version
            .as_ref()
            .is_some_and(|v| v.block == crate::BLOCK_PROTOCOL),
        "invalid light block protocol"
    );
    ensure!(
        header.chain_id.len() <= 50 && header.height > 0,
        "invalid light header chain or height"
    );
    time(header.time)?;
    basic_id(header.last_block_id.as_ref())?;
    for hash in [
        &header.last_commit_hash,
        &header.data_hash,
        &header.evidence_hash,
        &header.validators_hash,
        &header.next_validators_hash,
        &header.consensus_hash,
        &header.last_results_hash,
    ] {
        hash_size(hash)?;
    }
    ensure!(
        header.proposer_address.len() == 20,
        "invalid light header proposer address"
    );
    Ok(())
}

pub(super) fn basic_commit(commit: &types::Commit) -> Result<()> {
    ensure!(
        commit.height >= 0 && commit.round >= 0,
        "negative commit height or round"
    );
    basic_id(commit.block_id.as_ref())?;
    if commit.height > 0 {
        let id = normalize_id(commit.block_id.clone());
        let parts = id.part_set_header.as_ref().expect("normalized PartSet");
        ensure!(
            !(id.hash.is_empty() && parts.total == 0 && parts.hash.is_empty()),
            "commit for nil block"
        );
        ensure!(!commit.signatures.is_empty(), "missing commit signatures");
    }
    for sig in &commit.signatures {
        let timestamp = time(sig.timestamp)?;
        match flag(sig) {
            1 => ensure!(
                sig.validator_address.is_empty()
                    && sig.signature.is_empty()
                    && timestamp == ZERO_TIME,
                "absent commit signature contains address, time or signature"
            ),
            2 | 3 => ensure!(
                sig.validator_address.len() == 20
                    && !sig.signature.is_empty()
                    && sig.signature.len() <= 64,
                "invalid commit signature address or length"
            ),
            _ => anyhow::bail!("unknown commit signature flag"),
        }
    }
    Ok(())
}

fn flag(sig: &types::CommitSig) -> u8 {
    sig.block_id_flag as u8
}

pub(super) fn normalize_commit(commit: &mut types::Commit) {
    commit.block_id = Some(normalize_id(commit.block_id.take()));
    for sig in &mut commit.signatures {
        sig.block_id_flag = i32::from(flag(sig));
        sig.timestamp.get_or_insert(ZERO_TIME);
    }
}

pub(super) fn header_fields(header: &mut types::Header) {
    header.version.get_or_insert_with(Default::default);
    header.time.get_or_insert(ZERO_TIME);
    header.last_block_id = Some(normalize_id(header.last_block_id.take()));
}

pub(super) fn validator_address(val: &types::Validator) -> Result<Vec<u8>> {
    let key = val
        .pub_key
        .as_ref()
        .and_then(|key| key.sum.as_ref())
        .context("missing public key")?;
    Ok(match key {
        Key::Ed25519(bytes) => Hasher::digest(bytes)[..20].to_vec(),
        Key::Secp256k1(bytes) => {
            <ripemd::Ripemd160 as sha2::Digest>::digest(Hasher::digest(bytes)).to_vec()
        }
    })
}

fn basic_key(validator: &types::Validator) -> Result<()> {
    match validator.pub_key.as_ref().and_then(|key| key.sum.as_ref()) {
        Some(Key::Ed25519(key)) => ensure!(key.len() == 32, "invalid Ed25519 key length"),
        Some(Key::Secp256k1(key)) => ensure!(key.len() == 33, "invalid secp256k1 key length"),
        None => anyhow::bail!("missing validator public key"),
    }
    Ok(())
}

fn basic_validator(validator: &types::Validator) -> Result<()> {
    basic_key(validator)?;
    ensure!(
        validator.address.len() == 20 && validator.voting_power >= 0,
        "invalid light validator address or power"
    );
    Ok(())
}

fn total(set: &types::ValidatorSet) -> Result<i64> {
    let total = set.validators.iter().try_fold(0i64, |sum, val| {
        sum.checked_add(val.voting_power)
            .context("validator power overflow")
    })?;
    ensure!(
        (0..=i64::MAX / 8).contains(&total),
        "light validator power exceeds Go limit"
    );
    Ok(total)
}

fn set_hash(set: &types::ValidatorSet) -> [u8; 32] {
    let bytes = set
        .validators
        .iter()
        .map(|val| {
            types::SimpleValidator {
                pub_key: val.pub_key.clone(),
                voting_power: val.voting_power,
            }
            .encode_to_vec()
        })
        .collect::<Vec<_>>();
    tendermint::merkle::simple_hash_from_byte_vectors::<Hasher>(&bytes)
}

pub(super) fn basic(proof: &types::LightClientAttackEvidence) -> Result<()> {
    ensure!(
        proof.common_height > 0 && proof.total_voting_power > 0,
        "invalid light evidence height or power"
    );
    time(proof.timestamp)?;
    let block = proof
        .conflicting_block
        .as_ref()
        .context("missing conflicting light block")?;
    let signed = block
        .signed_header
        .as_ref()
        .context("missing conflicting signed header")?;
    let header = signed
        .header
        .as_ref()
        .context("missing conflicting header")?;
    let commit = signed
        .commit
        .as_ref()
        .context("missing conflicting commit")?;
    basic_header(header)?;
    basic_commit(commit)?;
    ensure!(
        proof.common_height <= header.height && commit.height == header.height,
        "light header height mismatch"
    );
    let expected_hash = header_hash(header)?;
    ensure!(
        commit
            .block_id
            .as_ref()
            .is_some_and(|id| id.hash == expected_hash),
        "light commit differs from header"
    );
    let set = block
        .validator_set
        .as_ref()
        .context("missing conflicting validators")?;
    ensure!(
        !set.validators.is_empty(),
        "empty conflicting validator set"
    );
    for val in &set.validators {
        basic_validator(val)?;
    }
    basic_validator(
        set.proposer
            .as_ref()
            .context("missing validator proposer")?,
    )?;
    total(set)?;
    ensure!(
        header.validators_hash == set_hash(set),
        "light validator set hash mismatch"
    );
    // Go FromProto decodes these keys but does not call Validator.ValidateBasic.
    for val in &proof.byzantine_validators {
        basic_key(val)?;
    }
    Ok(())
}

pub(super) fn basic_vote(vote: &mut types::Vote) -> Result<()> {
    ensure!(
        matches!(vote.r#type, 1 | 2)
            && vote.height >= 0
            && vote.round >= 0
            && vote.validator_index >= 0
            && vote.validator_address.len() == 20
            && !vote.signature.is_empty()
            && vote.signature.len() <= 64,
        "invalid evidence vote"
    );
    basic_id(vote.block_id.as_ref())?;
    let id = normalize_id(vote.block_id.take());
    let parts = id.part_set_header.as_ref().expect("normalized PartSet");
    ensure!(
        (id.hash.is_empty() && parts.total == 0 && parts.hash.is_empty())
            || (id.hash.len() == 32 && parts.total > 0 && parts.hash.len() == 32),
        "incomplete vote BlockID"
    );
    vote.block_id = Some(id);
    vote.timestamp = Some(time(vote.timestamp)?);
    Ok(())
}

pub(super) fn message_fields(proof: &mut types::LightClientAttackEvidence) {
    proof.timestamp.get_or_insert(ZERO_TIME);
    if let Some(block) = &mut proof.conflicting_block
        && let Some(signed) = &mut block.signed_header
    {
        if let Some(header) = &mut signed.header {
            header_fields(header);
        }
        if let Some(commit) = &mut signed.commit {
            commit.block_id = Some(normalize_id(commit.block_id.take()));
            for sig in &mut commit.signatures {
                sig.timestamp.get_or_insert(ZERO_TIME);
            }
        }
    }
}

pub(super) fn normalize(proof: &mut types::LightClientAttackEvidence) {
    message_fields(proof);
    if let Some(block) = &mut proof.conflicting_block {
        if let Some(set) = &mut block.validator_set {
            set.total_voting_power = 0;
        }
        if let Some(commit) = block
            .signed_header
            .as_mut()
            .and_then(|signed| signed.commit.as_mut())
        {
            normalize_commit(commit);
        }
    }
}

pub(super) fn vote_fields(vote: &mut types::Vote) {
    vote.block_id = Some(normalize_id(vote.block_id.take()));
    vote.timestamp.get_or_insert(ZERO_TIME);
}

fn sign_bytes(
    chain: &chain::Id,
    commit: &types::Commit,
    sig: &types::CommitSig,
) -> Result<Vec<u8>> {
    let id = normalize_id(commit.block_id.clone());
    let parts = id.part_set_header.context("missing PartSet")?;
    Ok(types::CanonicalVote {
        r#type: 2,
        height: commit.height,
        round: i64::from(commit.round),
        block_id: Some(types::CanonicalBlockId {
            hash: id.hash,
            part_set_header: Some(types::CanonicalPartSetHeader {
                total: parts.total,
                hash: parts.hash,
            }),
        }),
        timestamp: Some(time(sig.timestamp)?),
        chain_id: chain.to_string(),
    }
    .encode_length_delimited_to_vec())
}

fn verify_signature(val: &types::Validator, bytes: &[u8], signature: &[u8]) -> Result<()> {
    match val
        .pub_key
        .as_ref()
        .and_then(|key| key.sum.as_ref())
        .context("missing public key")?
    {
        Key::Ed25519(key) => {
            use tendermint::crypto::signature::Verifier;
            let public =
                tendermint::PublicKey::from_raw_ed25519(key).context("invalid Ed25519 key")?;
            let signature = tendermint::Signature::try_from(signature.to_vec())?;
            GoEd25519::verify(public, bytes, &signature)
                .map_err(|error| anyhow::anyhow!("invalid signature: {error:?}"))?;
        }
        Key::Secp256k1(key) => {
            let key = k256::ecdsa::VerifyingKey::from_sec1_bytes(key)?;
            let signature = k256::ecdsa::Signature::from_slice(signature)?;
            ensure!(
                signature.normalize_s() == signature,
                "non-canonical secp256k1 signature"
            );
            key.verify_prehash(&Hasher::digest(bytes), &signature)?;
        }
    }
    Ok(())
}

fn overlap(chain: &chain::Id, common: &validator::Set, commit: &types::Commit) -> Result<()> {
    let mut power = 0u64;
    let mut seen = BTreeSet::new();
    for sig in &commit.signatures {
        if flag(sig) != 2 {
            continue;
        }
        let Some(val) = common
            .validators
            .iter()
            .find(|val| val.address.as_bytes() == sig.validator_address)
        else {
            continue;
        };
        ensure!(
            seen.insert(val.address),
            "duplicate common validator signature"
        );
        verify_signature(
            &val.clone().into(),
            &sign_bytes(chain, commit, sig)?,
            &sig.signature,
        )?;
        power += val.power();
        if power > common.total_voting_power.value() / 3 {
            return Ok(());
        }
    }
    anyhow::bail!("insufficient common light voting power")
}

fn light_commit(
    chain: &chain::Id,
    set: &types::ValidatorSet,
    commit: &types::Commit,
) -> Result<()> {
    ensure!(
        set.validators.len() == commit.signatures.len(),
        "incorrect light commit size"
    );
    let needed = total(set)? * 2 / 3;
    let mut power = 0;
    for (val, sig) in set.validators.iter().zip(&commit.signatures) {
        if flag(sig) != 2 {
            continue;
        }
        // Go uses the key at this index; address is not part of vote sign bytes.
        verify_signature(val, &sign_bytes(chain, commit, sig)?, &sig.signature)?;
        power += val.voting_power;
        if power > needed {
            return Ok(());
        }
    }
    anyhow::bail!("insufficient light commit voting power")
}

pub(super) fn verify(
    proof: &types::LightClientAttackEvidence,
    common: &validator::Set,
    trusted_header: &block::Header,
    trusted_commit: &Commit,
) -> Result<()> {
    basic(proof)?;
    let block = proof
        .conflicting_block
        .as_ref()
        .context("missing light block")?;
    let signed = block
        .signed_header
        .as_ref()
        .context("missing signed header")?;
    let header = signed.header.as_ref().context("missing header")?;
    let commit = signed.commit.as_ref().context("missing commit")?;
    let set = block.validator_set.as_ref().context("missing validators")?;
    let invalid = header.validators_hash != trusted_header.validators_hash.as_bytes()
        || header.next_validators_hash != trusted_header.next_validators_hash.as_bytes()
        || header.consensus_hash != trusted_header.consensus_hash.as_bytes()
        || header.app_hash != trusted_header.app_hash.as_bytes()
        || header.last_results_hash
            != trusted_header
                .last_results_hash
                .unwrap_or_default()
                .as_bytes();
    if proof.common_height != header.height {
        overlap(&trusted_header.chain_id, common, commit)?;
    } else {
        ensure!(
            !invalid,
            "conflicting header cannot derive from the common state"
        );
    }
    light_commit(&trusted_header.chain_id, set, commit)?;
    ensure!(
        proof.total_voting_power == i64::from(common.total_voting_power),
        "incorrect common voting power"
    );
    let header_time: Time = time(header.time)?.try_into()?;
    ensure!(
        header.height <= i64::from(trusted_header.height) || header_time <= trusted_header.time,
        "conflicting header preserves monotonically increasing time"
    );
    ensure!(
        header_hash(header)?.as_slice() != trusted_header.hash().as_bytes(),
        "conflicting header equals trusted header"
    );
    let mut accused: Vec<types::Validator> = Vec::new();
    if invalid {
        for sig in &commit.signatures {
            if flag(sig) == 2
                && let Some(val) = common
                    .validators
                    .iter()
                    .find(|val| val.address.as_bytes() == sig.validator_address)
            {
                accused.push(val.clone().into());
            }
        }
    } else if commit.round == i32::from(trusted_commit.round) {
        ensure!(
            commit.signatures.len() == trusted_commit.signatures.len(),
            "trusted and conflicting commit sizes differ"
        );
        for (candidate, trusted) in commit.signatures.iter().zip(&trusted_commit.signatures) {
            if flag(candidate) != 1 && !trusted.is_absent() {
                accused.push(
                    set.validators
                        .iter()
                        .find(|val| val.address == candidate.validator_address)
                        .context("unknown conflicting signature address")?
                        .clone(),
                );
            }
        }
    } else {
        anyhow::bail!("Tendermint 0.34.19 rejects amnesia evidence decoded from the wire");
    }
    accused.sort_by_key(|val| (Reverse(val.voting_power), val.address.clone()));
    ensure!(
        accused.len() == proof.byzantine_validators.len()
            && accused
                .iter()
                .zip(&proof.byzantine_validators)
                .all(|(wanted, got)| wanted.address == got.address
                    && wanted.voting_power == got.voting_power),
        "incorrect byzantine validator list"
    );
    Ok(())
}
