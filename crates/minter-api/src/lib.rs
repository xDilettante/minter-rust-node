mod events;
mod http;

use prost::{Message, Name};
use prost_reflect::{
    DescriptorPool, DeserializeOptions, DynamicMessage, Kind, MessageDescriptor, ReflectMessage,
    SerializeOptions,
};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, sync::LazyLock};
use tokio::net::TcpListener;
use tonic::{Code, Status};

pub use events::{decode_events, encode_events, event_struct};
pub use http::router;
pub use prost_types::Any;

pub mod pb {
    tonic::include_proto!("api_pb");
}

mod google_rpc {
    tonic::include_proto!("google.rpc");
}

static SCHEMA: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(include_bytes!("../schema/minter-v2.pb").as_slice())
        .expect("the pinned Minter v2 descriptor set")
});

fn json<M: Message + Name>(message: &M) -> Result<Value, Status> {
    let name = M::full_name();
    let descriptor = SCHEMA
        .get_message_by_name(&name)
        .ok_or_else(|| Status::internal("missing API response descriptor"))?;
    let mut dynamic = DynamicMessage::new(descriptor);
    dynamic
        .transcode_from(message)
        .map_err(|_| Status::internal("cannot decode API response"))?;
    dynamic_json(dynamic)
}

fn dynamic_json(dynamic: DynamicMessage) -> Result<Value, Status> {
    let mut value = dynamic
        .serialize_with_options(
            serde_json::value::Serializer,
            &SerializeOptions::new()
                .use_proto_field_name(true)
                .skip_default_fields(false),
        )
        .map_err(|_| Status::internal("cannot encode API response"))?;
    emit_unset_messages(&dynamic.descriptor(), &mut value);
    Ok(value)
}

fn parse_json<M: Message + Name + Default>(bytes: &[u8]) -> Result<M, Status> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(M::default());
    }
    let descriptor = SCHEMA
        .get_message_by_name(&M::full_name())
        .ok_or_else(|| Status::internal("missing API request descriptor"))?;
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let message = DynamicMessage::deserialize_with_options(
        descriptor,
        &mut parser,
        &DeserializeOptions::new().deny_unknown_fields(false),
    )
    .map_err(|error| Status::invalid_argument(error.to_string()))?;
    parser
        .end()
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
    message
        .transcode_to()
        .map_err(|_| Status::internal("cannot decode API request"))
}

pub fn pack<M: Message + Name>(message: M) -> Any {
    Any {
        type_url: format!("type.googleapis.com/{}", M::full_name()),
        value: message.encode_to_vec(),
    }
}

pub fn protobuf_struct(value: Value) -> Result<prost_types::Struct, Status> {
    DynamicMessage::deserialize(
        SCHEMA
            .get_message_by_name("google.protobuf.Struct")
            .expect("pinned protobuf Struct"),
        value,
    )
    .map_err(|_| Status::internal("invalid protobuf Struct"))?
    .transcode_to()
    .map_err(|_| Status::internal("cannot decode protobuf Struct"))
}

fn emit_unset_messages(descriptor: &MessageDescriptor, value: &mut Value) {
    if descriptor.full_name().starts_with("google.protobuf.") {
        return;
    }
    let Some(fields) = value.as_object_mut() else {
        return;
    };
    for field in descriptor.fields() {
        let Kind::Message(message) = field.kind() else {
            continue;
        };
        let Some(value) = fields.get_mut(field.name()) else {
            // Go's EmitUnpopulated writes null for absent messages outside oneofs.
            if field.containing_oneof().is_none() && !field.is_list() && !field.is_map() {
                fields.insert(field.name().into(), Value::Null);
            }
            continue;
        };
        if field.is_map() {
            if let (Some(map), Kind::Message(element)) = (
                value.as_object_mut(),
                message.map_entry_value_field().kind(),
            ) {
                for value in map.values_mut() {
                    emit_unset_messages(&element, value);
                }
            }
        } else if let Some(list) = value.as_array_mut() {
            for value in list {
                emit_unset_messages(&message, value);
            }
        } else {
            emit_unset_messages(&message, value);
        }
    }
}

pub async fn serve_grpc(
    listener: TcpListener,
    service: impl pb::api_service_server::ApiService,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(pb::api_service_server::ApiServiceServer::new(service))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            shutdown,
        )
        .await
}

pub async fn serve_http(
    listener: TcpListener,
    service: impl pb::api_service_server::ApiService + Clone,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(service))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Minter error 102 with the google.protobuf.Struct used by the Go API.
pub fn coin_not_found(field: &str, value: String) -> Status {
    let string = |value: String| prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value)),
    };
    let mut fields = BTreeMap::from([("code".into(), string("102".into()))]);
    if !value.is_empty() {
        fields.insert(field.into(), string(value));
    }
    status_with_details(
        Code::NotFound,
        "Coin not found",
        prost_types::Struct { fields },
    )
}

/// Pass CheckTx.Info through the same status-details format as the Go service.
pub fn transaction_rejected(message: &str, info: &str) -> Status {
    let Some(detail) = serde_json::from_str(info)
        .ok()
        .and_then(|value| protobuf_struct(value).ok())
    else {
        return Status::invalid_argument(message);
    };
    status_with_details(Code::InvalidArgument, message, detail)
}

fn status_with_details(code: Code, message: &str, detail: prost_types::Struct) -> Status {
    let details = google_rpc::Status {
        code: code as i32,
        message: message.into(),
        details: vec![Any {
            type_url: "type.googleapis.com/google.protobuf.Struct".into(),
            value: detail.encode_to_vec(),
        }],
    };
    Status::with_details(code, message, details.encode_to_vec().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_go_defaults_and_preserves_descriptor_enum_names() {
        let address = json(&pb::AddressResponse::default()).unwrap();
        assert_eq!(address["transaction_count"], "0");
        assert_eq!(address["balance"], serde_json::json!([]));
        assert_eq!(address.get("multisig"), Some(&Value::Null));
        let coin = json(&pb::CoinInfoResponse::default()).unwrap();
        assert_eq!(coin.get("owner_address"), Some(&Value::Null));
        let transaction = SCHEMA
            .get_enum_by_name("api_pb.TransactionResponse.Type")
            .unwrap();
        assert_eq!(transaction.get_value(0).unwrap().name(), "_");
        assert_eq!(pb::transaction_response::Type::Unspecified as i32, 0);
    }
}
