//! Tendermint 0.34 TCP private validator protocol.

use crate::{
    Endpoint,
    server::node_key_at,
    wire::{DeadlineStream, read_frame, set_deadline, write_frame},
};
use anyhow::{Context, Result, ensure};
use ed25519_consensus::SigningKey;
use std::{
    net::TcpStream,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tendermint::node;
use tendermint_p2p::secret_connection::PublicKey;
use tendermint_p2p::secret_connection::{SecretConnection, Version};
use tendermint_proto::v0_34::privval;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 10 * 1024;

pub struct SignerConnection {
    stream: SecretConnection<DeadlineStream>,
    read_deadline: Arc<Mutex<Instant>>,
    write_deadline: Arc<Mutex<Instant>>,
}

pub struct SignerIdentity(SigningKey);

impl SignerIdentity {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self(node_key_at(path)?))
    }

    pub fn id(&self) -> node::Id {
        PublicKey::from(&self.0).peer_id()
    }
}

impl SignerConnection {
    pub fn connect(peer: Endpoint, identity: &SignerIdentity) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&peer.address, CONNECT_TIMEOUT)
            .context("connect to Tendermint validator")?;
        stream.set_nodelay(true)?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let read_deadline = Arc::new(Mutex::new(deadline));
        let write_deadline = Arc::new(Mutex::new(deadline));
        let stream = SecretConnection::new(
            DeadlineStream::new(stream, read_deadline.clone(), write_deadline.clone())?,
            identity.0.clone(),
            Version::V0_34,
        )
        .context("private validator Secret Connection handshake")?;
        ensure!(
            stream.remote_pubkey().peer_id() == peer.id,
            "private validator listener public key does not match {}",
            peer.id
        );
        Ok(Self {
            stream,
            read_deadline,
            write_deadline,
        })
    }

    pub fn receive(&mut self) -> Result<privval::Message> {
        set_deadline(&self.read_deadline, READ_TIMEOUT)?;
        read_frame(&mut self.stream, MAX_MESSAGE).context("read private validator request")
    }

    pub fn send(&mut self, response: privval::Message) -> Result<()> {
        set_deadline(&self.write_deadline, WRITE_TIMEOUT)?;
        write_frame(&mut self.stream, &response).context("write private validator response")
    }
}
