use crate::{Any, SCHEMA, dynamic_json, protobuf_struct};
use prost::Message;
use prost_reflect::DynamicMessage;
use prost_types::Struct;
use serde_json::json;
use std::collections::HashSet;
use tonic::Status;

#[derive(Message)]
struct EventBatch {
    #[prost(message, repeated, tag = "1")]
    events: Vec<Any>,
}

pub fn encode_events(events: Vec<Any>) -> Vec<u8> {
    EventBatch { events }.encode_to_vec()
}

pub fn decode_events(bytes: &[u8]) -> Result<Vec<Any>, prost::DecodeError> {
    Ok(EventBatch::decode(bytes)?.events)
}

/// Go's Events API uses a Tendermint JSON envelope around each Minter event.
pub fn event_struct(event: Any, search: &HashSet<&str>) -> Result<Option<Struct>, Status> {
    let name = event
        .type_url
        .strip_prefix("type.googleapis.com/api_pb.")
        .ok_or_else(|| Status::internal("invalid Minter event type"))?;
    if !search.is_empty()
        && !matches!(
            name,
            "RewardEvent" | "SlashEvent" | "UnbondEvent" | "StakeKickEvent"
        )
    {
        return Ok(None);
    }
    let descriptor = SCHEMA
        .get_message_by_name(&format!("api_pb.{name}"))
        .ok_or_else(|| Status::internal("unknown Minter event type"))?;
    let event = DynamicMessage::decode(descriptor, event.value.as_slice())
        .map_err(|_| Status::internal("invalid Minter event payload"))?;
    let value = dynamic_json(event)?;
    if !search.is_empty()
        && !["address", "validator_pub_key"]
            .iter()
            .filter_map(|key| value[*key].as_str())
            .any(|text| search.contains(text))
    {
        return Ok(None);
    }
    protobuf_struct(json!({"type": format!("minter/{name}"), "value": value})).map(Some)
}
