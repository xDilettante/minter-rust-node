use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use minter_p2p::Peer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::NamedTempFile;

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Report the authenticated peer and the range it advertises.
    Status(ConnectionArgs),
    /// Fetch blocks over one connection and check their transaction roots.
    Block {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(long, required = true, value_parser = clap::value_parser!(u64).range(1..))]
        height: Vec<u64>,
        /// Write the length-delimited BlockResponses to a new file.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args)]
pub(crate) struct ConnectionArgs {
    #[arg(long, value_name = "ID@IP:PORT")]
    peer: String,
    #[arg(long, default_value = "minter-mainnet-5")]
    chain: String,
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_secs: u64,
}

impl ConnectionArgs {
    fn connect(&self) -> Result<Peer> {
        let chain = self.chain.parse().context("invalid chain ID")?;
        Peer::connect(
            self.peer.parse()?,
            &chain,
            Duration::from_secs(self.timeout_secs),
        )
    }
}

pub(crate) fn run(action: Action) -> Result<()> {
    let report = match action {
        Action::Status(args) => status(&mut args.connect()?)?,
        Action::Block {
            connection,
            height,
            output,
        } => {
            let mut peer = connection.connect()?;
            let status = status(&mut peer)?;
            let mut file = output.as_deref().map(new_capture).transpose()?;
            let mut blocks = Vec::with_capacity(height.len());
            for height in height {
                let received = peer
                    .block(height.try_into()?)?
                    .with_context(|| format!("peer has no block at height {height}"))?;
                if let Some(file) = &mut file {
                    received.write(file)?;
                }
                let block = received.block();
                let header = &block.header;
                let id = received.id();
                blocks.push(json!({
                    "peerId": peer.node_info().default_node_id,
                    "chainId": header.chain_id.as_str(),
                    "height": height,
                    "blockHash": hex::encode(id.hash.as_bytes()),
                    "parts": id.part_set_header.total,
                    "partSetHash": hex::encode(id.part_set_header.hash.as_bytes()),
                    "appHash": hex::encode(header.app_hash.as_bytes()),
                    "transactions": block.data.len(),
                    "wireSha256": hex::encode(Sha256::digest(received.wire())),
                }));
            }
            if let (Some(file), Some(path)) = (file, output) {
                file.as_file().sync_all()?;
                file.persist_noclobber(&path)
                    .with_context(|| format!("publish block response to {}", path.display()))?;
            }
            json!({ "peer": status, "blocks": blocks })
        }
    };
    println!("{report}");
    Ok(())
}

fn status(peer: &mut Peer) -> Result<Value> {
    let range = peer.status()?;
    let info = peer.node_info();
    Ok(json!({
        "peerId": info.default_node_id,
        "chainId": info.network,
        "version": info.version,
        "base": range.base,
        "height": range.height,
    }))
}

fn new_capture(path: &Path) -> Result<NamedTempFile> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(NamedTempFile::new_in(parent)?)
}
