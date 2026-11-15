use minter_api::{pb, protobuf_struct};
use serde::Deserialize;
use serde_json::{Number, Value};
use std::collections::BTreeMap;
use tonic::Status;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireResult {
    query: Option<String>,
    data: Option<Tagged>,
    events: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Deserialize)]
struct Tagged {
    #[serde(rename = "type")]
    kind: String,
    value: Value,
}

impl WireResult {
    pub(super) fn response(
        self,
        subscribed: &str,
    ) -> Result<Option<pb::SubscribeResponse>, Status> {
        let (query, data, events) = match (self.query, self.data, self.events) {
            (None, None, None) => return Ok(None), // Subscribe acknowledgement.
            (Some(query), Some(data), Some(events)) if query == subscribed => (query, data, events),
            _ => return Err(invalid_event()),
        };
        Ok(Some(pb::SubscribeResponse {
            query,
            data: Some(protobuf_struct(data.minter_json()?)?),
            events: events
                .into_iter()
                .map(|(key, events)| pb::subscribe_response::Event { key, events })
                .collect(),
        }))
    }
}

impl Tagged {
    /// Tendermint RPC uses tmjson. The Go Minter service uses encoding/json
    /// on the same Go types, so only the affected fields need projection.
    fn minter_json(self) -> Result<Value, Status> {
        let mut data = self.value;
        match self.kind.as_str() {
            "tendermint/event/NewBlock" => {
                if let Some(block) = data.get_mut("block") {
                    header_and_commit(block, "/last_commit/height")?;
                    for item in array(block.pointer_mut("/evidence/evidence")) {
                        evidence(item)?;
                    }
                }
                block_results(&mut data)?;
            }
            "tendermint/event/NewBlockHeader" => {
                numbers(
                    &mut data,
                    &[
                        "/header/height",
                        "/header/version/block",
                        "/header/version/app",
                        "/num_txs",
                    ],
                )?;
                block_results(&mut data)?;
            }
            "tendermint/event/Tx" => {
                let Value::Object(mut outer) = data else {
                    return Err(invalid_event());
                };
                data = outer.remove("TxResult").ok_or_else(invalid_event)?;
                numbers(
                    &mut data,
                    &["/height", "/result/gas_wanted", "/result/gas_used"],
                )?;
                omit_empty(&mut data, "tx");
                if let Some(result) = data.get_mut("result") {
                    omit_empty(result, "data");
                    abci_events(result);
                }
            }
            "tendermint/event/NewEvidence" => {
                numbers(&mut data, &["/height"])?;
                evidence(data.get_mut("evidence").ok_or_else(invalid_event)?)?;
            }
            "tendermint/event/ValidatorSetUpdates" => {
                for update in array(data.get_mut("validator_updates")) {
                    validator(update)?;
                }
            }
            "tendermint/event/Vote" => numbers(&mut data, &["/Vote/height"])?,
            "tendermint/event/RoundState"
            | "tendermint/event/NewRound"
            | "tendermint/event/CompleteProposal" => numbers(&mut data, &["/height"])?,
            _ => return Err(Status::internal("unsupported Tendermint 0.34 event type")),
        }
        if !data.is_object() {
            return Err(invalid_event());
        }
        Ok(data)
    }
}

fn header_and_commit(value: &mut Value, commit_height: &str) -> Result<(), Status> {
    numbers(
        value,
        &[
            "/header/height",
            "/header/version/block",
            "/header/version/app",
            commit_height,
        ],
    )
}

fn block_results(data: &mut Value) -> Result<(), Status> {
    if let Some(begin) = data.get_mut("result_begin_block") {
        abci_events(begin);
    }
    if let Some(end) = data.get_mut("result_end_block") {
        abci_events(end);
        for update in array(end.get_mut("validator_updates")) {
            numbers(update, &["/power"])?;
            if let Some(sum) = update
                .pointer_mut("/pub_key/Sum")
                .filter(|sum| !sum.is_null())
            {
                untag(sum)?;
            }
        }
        if let Some(params) = end.get_mut("consensus_param_updates") {
            numbers(
                params,
                &[
                    "/block/max_bytes",
                    "/block/max_gas",
                    "/evidence/max_age_num_blocks",
                    "/evidence/max_age_duration",
                    "/evidence/max_bytes",
                    "/version/app_version",
                ],
            )?;
            if let Some(validator) = params.get_mut("validator") {
                omit_empty(validator, "pub_key_types");
            }
        }
    }
    Ok(())
}

fn abci_events(data: &mut Value) {
    for event in array(data.get_mut("events")) {
        omit_empty(event, "attributes");
    }
    omit_empty(data, "events");
}

fn evidence(value: &mut Value) -> Result<(), Status> {
    if value.is_null() {
        return Ok(());
    }
    match untag(value)?.as_str() {
        "tendermint/DuplicateVoteEvidence" => numbers(
            value,
            &[
                "/vote_a/height",
                "/vote_b/height",
                "/TotalVotingPower",
                "/ValidatorPower",
            ],
        ),
        "tendermint/LightClientAttackEvidence" => {
            numbers(value, &["/CommonHeight", "/TotalVotingPower"])?;
            for row in array(value.get_mut("ByzantineValidators")) {
                validator(row)?;
            }
            if let Some(header) = value.pointer_mut("/ConflictingBlock/signed_header") {
                header_and_commit(header, "/commit/height")?;
            }
            if let Some(set) = value.pointer_mut("/ConflictingBlock/validator_set") {
                for row in array(set.get_mut("validators")) {
                    validator(row)?;
                }
                if let Some(proposer) = set.get_mut("proposer") {
                    validator(proposer)?;
                }
            }
            Ok(())
        }
        _ => Err(invalid_event()),
    }
}

fn validator(value: &mut Value) -> Result<(), Status> {
    numbers(value, &["/voting_power", "/proposer_priority"])?;
    if let Some(key) = value.get_mut("pub_key").filter(|key| !key.is_null()) {
        untag(key)?;
    }
    Ok(())
}

fn untag(value: &mut Value) -> Result<String, Status> {
    let tagged: Tagged =
        serde_json::from_value(std::mem::take(value)).map_err(|_| invalid_event())?;
    *value = tagged.value;
    Ok(tagged.kind)
}

fn numbers(data: &mut Value, paths: &[&str]) -> Result<(), Status> {
    for path in paths {
        if let Some(value) = data.pointer_mut(path) {
            let integer = value
                .as_str()
                .and_then(|text| text.parse::<Number>().ok())
                .filter(|n| n.is_i64() || n.is_u64())
                .ok_or_else(invalid_event)?;
            *value = Value::Number(integer);
        }
    }
    Ok(())
}

fn array(value: Option<&mut Value>) -> impl Iterator<Item = &mut Value> {
    value.and_then(Value::as_array_mut).into_iter().flatten()
}

fn omit_empty(data: &mut Value, field: &str) {
    if let Some(object) = data.as_object_mut()
        && object.get(field).is_some_and(|value| {
            value.as_array().is_some_and(Vec::is_empty) || value.as_str().is_some_and(str::is_empty)
        })
    {
        object.remove(field);
    }
}

fn invalid_event() -> Status {
    Status::internal("invalid Tendermint subscription event")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Vector {
        wire: WireResult,
        data: Value,
    }

    #[test]
    fn converts_tendermint_rpc_payloads_to_the_go_minter_structs() {
        for line in include_str!("../../../tests/fixtures/go-subscribe-v034.jsonl").lines() {
            let vector: Vector = serde_json::from_str(line).unwrap();
            let expected = protobuf_struct(vector.data).unwrap();
            let query = vector.wire.query.clone().unwrap();
            let response = vector.wire.response(&query).unwrap().unwrap();
            assert_eq!(response.data.unwrap(), expected, "{}", response.query);
        }
    }
}
