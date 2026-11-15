# Third-party notices

The original notices in vendored source and generated schemas remain applicable.

- Minter Go node is the protocol/reference implementation used for this port. Its MIT notice is retained in [licenses/minter-go-node-MIT.txt](licenses/minter-go-node-MIT.txt). Upstream: https://github.com/MinterTeam/minter-go-node.
- The local Tendermint P2P fork retains its [Apache-2.0 license](vendor/tendermint-p2p/LICENSE). Upstream: https://github.com/informalsystems/tendermint-rs.
- Minter API, Google APIs, protobuf and gRPC Gateway schema notices are retained in [crates/minter-api/schema](crates/minter-api/schema).
- Rust dependencies are specified by `Cargo.lock` and `desktop/src-tauri/Cargo.lock`; JavaScript dependencies by `desktop/package-lock.json`. Their respective licenses apply. The Inter and IBM Plex Mono font packages use SIL Open Font License terms; retain their notices when distributing a compiled GUI.
- `is-reference` 3.0.3 and `locate-character` 3.0.0 declare MIT in their upstream package metadata and README but do not include a separate license file. Version-specific attribution and standard MIT permission text are retained in [licenses/javascript](licenses/javascript); portable builds also retain the upstream package metadata and README.

This repository's MIT license covers the project's own code. It does not replace third-party notices or licenses.
