use super::report;
use anyhow::{Context, Result, anyhow};
use minter_app::native_sync::NativeNode;
use minter_p2p::{
    BlockRange, Endpoint, Peer, ReceivedBlock, mempool::Mempool, server::BlockServer,
};
use std::{collections::BTreeSet, thread, time::Duration};
use tendermint::{chain, node};

const RETRY_DELAY: Duration = Duration::from_secs(2);
const GOSSIP_DELAY: Duration = Duration::from_millis(250);

struct Connected {
    peer: Peer,
    range: BlockRange,
    window: Option<(ReceivedBlock, ReceivedBlock)>,
}

struct Entry {
    endpoint: Endpoint,
    connection: Option<Connected>,
    combined: bool,
}

pub(super) struct Peers<'a> {
    entries: Vec<Entry>,
    chain_id: chain::Id,
    timeout: Duration,
    server: Option<&'a BlockServer>,
}

impl<'a> Peers<'a> {
    pub(super) fn new(
        endpoints: Vec<Endpoint>,
        combined: BTreeSet<node::Id>,
        server: Option<&'a BlockServer>,
        chain_id: chain::Id,
        timeout: Duration,
    ) -> Self {
        Self {
            entries: endpoints
                .into_iter()
                .map(|endpoint| Entry {
                    combined: combined.contains(&endpoint.id),
                    endpoint,
                    connection: None,
                })
                .collect(),
            chain_id,
            timeout,
            server,
        }
    }

    pub(super) fn available(&mut self, start: u64, node: &NativeNode) -> Result<u64> {
        let mut available = None;
        for entry in &mut self.entries {
            match entry.connection(&self.chain_id, self.timeout, self.server, node) {
                Ok(connection)
                    if connection.range.base <= start && connection.range.height >= 2 =>
                {
                    let height = connection.range.height - 2;
                    available =
                        Some(available.map_or(height, |previous: u64| previous.max(height)));
                }
                Ok(_) => {}
                Err(error) => entry.failed(start, &error),
            }
        }
        available.context("no configured peer can supply the requested block range")
    }

    pub(super) fn run(
        &mut self,
        sync: &NativeNode,
        start: u64,
        through: Option<u64>,
        follow: bool,
        trace: bool,
        stopping: impl Fn() -> bool,
    ) -> Result<()> {
        let mut height = start;
        let mut preferred = 0;
        while !stopping() && through.is_none_or(|last| height <= last) {
            let mut advanced = false;
            let mut last_error = None;
            for offset in 0..self.entries.len() {
                if stopping() {
                    return Ok(());
                }
                let index = (preferred + offset) % self.entries.len();
                let entry = &mut self.entries[index];
                let combined = entry.combined;
                let result = entry
                    .connection(&self.chain_id, self.timeout, self.server, sync)
                    .and_then(|connection| connection.advance(sync, height, combined));
                match result {
                    Ok(Some(applied)) => {
                        report(&applied, trace || through == Some(height))?;
                        height += 1;
                        preferred = index;
                        advanced = true;
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        entry.failed(height, &error);
                        last_error = Some(error);
                    }
                }
            }
            if !advanced {
                if !follow {
                    return Err(last_error.unwrap_or_else(|| {
                        anyhow!("peers have no signed block at height {height}")
                    }))
                    .with_context(|| format!("no configured peer could advance height {height}"));
                }
                let sharing = self
                    .entries
                    .iter()
                    .any(|entry| entry.combined && entry.connection.is_some());
                thread::sleep(if sharing { GOSSIP_DELAY } else { RETRY_DELAY });
            }
        }
        Ok(())
    }
}

impl Entry {
    fn connection(
        &mut self,
        chain: &chain::Id,
        timeout: Duration,
        server: Option<&BlockServer>,
        node: &NativeNode,
    ) -> Result<&mut Connected> {
        if self.connection.is_none() {
            let (mut peer, gossip): (_, Option<&dyn Mempool>) = if self.combined {
                let server =
                    server.context("shared block and mempool connection requires a listener")?;
                (server.connect_block_and_mempool(self.endpoint)?, Some(node))
            } else {
                (Peer::connect(self.endpoint, chain, timeout)?, None)
            };
            let range = peer.status_with(gossip)?;
            self.connection = Some(Connected {
                peer,
                range,
                window: None,
            });
        }
        self.connection
            .as_mut()
            .context("peer connection unavailable")
    }

    fn failed(&mut self, height: u64, error: &anyhow::Error) {
        eprintln!(
            "Tendermint peer {}@{} failed at {height}: {error:#}",
            self.endpoint.id, self.endpoint.address
        );
        self.connection = None;
    }
}

impl Connected {
    fn advance(
        &mut self,
        sync: &NativeNode,
        height: u64,
        combined: bool,
    ) -> Result<Option<minter_app::native_sync::AppliedBlock>> {
        let gossip = combined.then_some(sync as &dyn Mempool);
        if self.range.base > height || self.range.height < height + 2 {
            self.range = self.peer.status_with(gossip)?;
            if self.range.base > height || self.range.height < height + 2 {
                return Ok(None);
            }
        }
        let (first, next) = match self.window.take() {
            Some((first, next)) if first.block().header.height.value() == height => (first, next),
            _ => (self.block(height, gossip)?, self.block(height + 1, gossip)?),
        };
        let witness = self.block(height + 2, gossip)?;
        let mut sync = sync.lock()?;
        let result = sync
            .apply(&first, &next, &witness)
            .with_context(|| format!("apply native block {height}"))?;
        sync.observe_peer_height(self.range.height.min(height.saturating_add(64)));
        self.window = Some((next, witness));
        Ok(Some(result))
    }

    fn block(&mut self, height: u64, gossip: Option<&dyn Mempool>) -> Result<ReceivedBlock> {
        self.peer
            .block_with(height.try_into()?, gossip)?
            .with_context(|| format!("peer has no block at height {height}"))
    }
}
