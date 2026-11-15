# Minter v2 schema

`minter-v2.pb` is a deterministic protobuf descriptor set exported from the Go dependency `github.com/MinterTeam/node-grpc-gateway v1.6.2-0.20220413090743-53ffbb191668`. The export includes the API service, request and response types, transaction data, events, HTTP annotations and the standard Google RPC status. [Minter API contract](https://raw.githubusercontent.com/MinterTeam/node-grpc-gateway/53ffbb191668/api.proto).

Regenerate from the pinned Minter Go module.

```text
go run -mod=readonly /path/to/tools/export_api_v2_schema.go /path/to/crates/minter-api/schema/minter-v2.pb
```

The descriptor SHA-256 is `4f4f9b52d4eb0348a0640906ef852218ba3fe5e6fa7c258701d7050d6162de56`. Tonic and Prost generate Rust types at build time from this file. The Rust generator aliases the transaction enum value `_` to `Unspecified`. ProtoJSON uses the original name and the wire number is zero. [Tonic descriptor compilation](https://docs.rs/tonic-prost-build/0.14.9/tonic_prost_build/struct.Builder.html#method.compile_fds).

The accompanying license files cover Minter, grpc-gateway v2.10.0, Go protobuf v1.27.1 and Google API descriptors from genproto `v0.0.0-20220317150908-0efb43f6373e`.
