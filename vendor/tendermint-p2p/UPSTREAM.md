# Tendermint P2P dependency

The source is from the published `tendermint-p2p` crate version 0.40.4 at commit
`d8c361ec5597e3f9a005d7211eec9c74eb9b04df`. The original crate SHA-256 is
`047c1002b9af811bfc62afef70be329e779a3e7f4a2fd23eebce102d1b63e5f4`.
[Upstream source](https://github.com/cometbft/tendermint-rs/tree/v0.40.4/p2p).

Minter changes only the receive buffer in `src/secret_connection.rs`. Reads copy
the available bytes and retain the remainder of a decrypted frame. An empty
read returns zero without consuming a frame. This keeps the byte-stream contract
when reading a Protobuf length prefix ahead of the rest of a packet.
