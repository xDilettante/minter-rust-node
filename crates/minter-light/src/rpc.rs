use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client, redirect::Policy};
use std::{collections::BTreeSet, io::Read, time::Duration};
use tendermint::{account, block::Height, chain, node, validator};
use tendermint_light_client::{
    components::io::{AtHeight, Io, IoError},
    types::LightBlock,
};
use tendermint_rpc::endpoint::{abci_query, commit, status, tx, validators};

const RPC_BYTES: u64 = 8 * 1024 * 1024;
const PROOF_BYTES: u64 = 32 * 1024 * 1024;
const MAX_VALIDATORS: i32 = 4096;

#[derive(Clone)]
pub(crate) struct Rpc {
    url: Url,
    client: Client,
}

#[derive(Clone)]
pub(crate) struct Peer {
    rpc: Rpc,
    pub id: node::Id,
    chain_id: chain::Id,
    pub latest: Height,
}

impl Rpc {
    pub fn new(endpoint: &str) -> Result<Self> {
        let url = Url::parse(endpoint).context("invalid Tendermint RPC URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Tendermint RPC requires an HTTP(S) base URL"
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(12))
            .redirect(Policy::none())
            .build()?;
        Ok(Self { url, client })
    }

    pub fn peer(self, chain_id: &chain::Id) -> Result<Peer> {
        let status: status::Response = self.get("status", &[], RPC_BYTES)?;
        ensure!(
            status.node_info.network == *chain_id,
            "RPC reports a different chain ID"
        );
        Ok(Peer {
            rpc: self,
            id: status.node_info.id,
            chain_id: chain_id.clone(),
            latest: status.sync_info.latest_block_height,
        })
    }

    pub fn same_endpoint(&self, other: &Self) -> bool {
        self.url.origin() == other.url.origin()
            && self.url.path().trim_end_matches('/') == other.url.path().trim_end_matches('/')
    }

    pub fn iavl(&self, height: u64, key: &[u8]) -> Result<abci_query::AbciQuery> {
        let response: abci_query::Response = self.get(
            "abci_query",
            &[
                ("path", &serde_json::to_string(minter_iavl::QUERY_PATH)?),
                ("data", &format!("0x{}", hex::encode(key))),
                ("height", &height.to_string()),
                ("prove", "true"),
            ],
            PROOF_BYTES,
        )?;
        Ok(response.response)
    }

    pub fn transaction(&self, hash: [u8; 32]) -> Result<tx::v0_34::DialectResponse> {
        self.get(
            "tx",
            &[
                ("hash", &format!("0x{}", hex::encode(hash))),
                ("prove", "true"),
            ],
            PROOF_BYTES,
        )
    }

    pub fn results(&self, height: Height) -> Result<crate::tx::Results> {
        self.get(
            "block_results",
            &[("height", &height.to_string())],
            PROOF_BYTES,
        )
    }

    fn get<R: tendermint_rpc::Response>(
        &self,
        method: &str,
        query: &[(&str, &str)],
        limit: u64,
    ) -> Result<R> {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Tendermint RPC base path"))?
            .pop_if_empty()
            .push(method);
        let response = self
            .client
            .get(url)
            .query(query)
            .send()?
            .error_for_status()?;
        ensure!(
            response.content_length().is_none_or(|bytes| bytes <= limit),
            "RPC response is too large"
        );
        let mut data = Vec::new();
        response.take(limit + 1).read_to_end(&mut data)?;
        ensure!(data.len() as u64 <= limit, "RPC response is too large");
        R::from_string(data).with_context(|| format!("Tendermint RPC {method}"))
    }
}

impl Peer {
    pub fn light_block(&self, height: Height) -> Result<LightBlock> {
        let reply: commit::Response =
            self.rpc
                .get("commit", &[("height", &height.to_string())], RPC_BYTES)?;
        let header = &reply.signed_header.header;
        ensure!(
            header.height == height && header.chain_id == self.chain_id,
            "RPC returned another height or chain"
        );
        let proposer = header.proposer_address;
        let current = self.validators(height)?;
        let next = Height::try_from(height.value().checked_add(1).context("height overflow")?)?;
        Ok(LightBlock::new(
            reply.signed_header,
            validator::Set::with_proposer(current, proposer)?,
            validator::Set::without_proposer(self.validators(next)?),
            self.id,
        ))
    }

    fn validators(&self, height: Height) -> Result<Vec<validator::Info>> {
        let mut all = Vec::new();
        let mut total = None;
        for page in 1.. {
            let reply: validators::Response = self.rpc.get(
                "validators",
                &[
                    ("height", &height.to_string()),
                    ("page", &page.to_string()),
                    ("per_page", "100"),
                ],
                RPC_BYTES,
            )?;
            ensure!(
                reply.block_height == height
                    && (1..=MAX_VALIDATORS).contains(&reply.total)
                    && total.is_none_or(|n| n == reply.total)
                    && all.len() < reply.total as usize
                    && reply.validators.len() == (reply.total as usize - all.len()).min(100),
                "invalid Tendermint validator page"
            );
            all.extend(reply.validators);
            total = Some(reply.total);
            if all.len() == reply.total as usize {
                break;
            }
        }
        let mut addresses = BTreeSet::new();
        let mut power = 0u64;
        for validator in &all {
            ensure!(
                validator.address == account::Id::from(validator.pub_key)
                    && addresses.insert(validator.address)
                    && validator.power.value() > 0,
                "invalid or duplicate Tendermint validator"
            );
            power = power
                .checked_add(validator.power.value())
                .context("validator power overflow")?;
        }
        ensure!(
            power <= validator::Set::MAX_TOTAL_VOTING_POWER,
            "validator power exceeds the Tendermint limit"
        );
        Ok(all)
    }
}

impl Io for Peer {
    fn fetch_light_block(&self, height: AtHeight) -> Result<LightBlock, IoError> {
        let height = match height {
            AtHeight::At(height) => height,
            AtHeight::Highest => self.latest,
        };
        self.light_block(height).map_err(|error| {
            IoError::rpc(tendermint_rpc::Error::invalid_params(format!("{error:#}")))
        })
    }
}
