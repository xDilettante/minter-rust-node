use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use minter_app::validator_signer::{SignError, ValidatorSigner};
use minter_p2p::{
    Endpoint,
    signer::{SignerConnection, SignerIdentity},
};
use std::{path::PathBuf, thread, time::Duration};
use tendermint::{account, chain};
use tendermint_proto::v0_34::privval::{self, message::Sum};

#[derive(Subcommand)]
pub(super) enum Command {
    /// Import the signing state of a stopped Go validator into a new journal directory.
    Import {
        #[command(flatten)]
        validator: Validator,
        #[arg(long)]
        go_state: PathBuf,
    },
    /// Sign requests from the authenticated Tendermint listener.
    Run {
        #[command(flatten)]
        validator: Validator,
        /// Tendermint Secret Connection listener identity and address, as ID@IP:port.
        #[arg(long)]
        peer: Endpoint,
        /// Separate Secret Connection transport key. Created if missing.
        #[arg(long)]
        transport_key: PathBuf,
    },
    /// Create or read the separate transport key and print its peer ID.
    TransportId {
        #[arg(long)]
        transport_key: PathBuf,
    },
}

#[derive(Args)]
pub(super) struct Validator {
    /// Tendermint chain ID.
    #[arg(long)]
    chain_id: chain::Id,
    /// Path to the Go-compatible private validator Ed25519 key.
    #[arg(long)]
    key: PathBuf,
    /// Dedicated RocksDB signing journal directory.
    #[arg(long)]
    journal: PathBuf,
}

pub(super) fn run(command: Command) -> Result<()> {
    match command {
        Command::Import {
            validator,
            go_state,
        } => ValidatorSigner::import(
            &validator.journal,
            &validator.key,
            &go_state,
            validator.chain_id,
        ),
        Command::Run {
            validator,
            peer,
            transport_key,
        } => serve(
            ValidatorSigner::open(&validator.journal, &validator.key, &validator.chain_id)?,
            peer,
            SignerIdentity::open(&transport_key)?,
        ),
        Command::TransportId { transport_key } => {
            println!("{}", SignerIdentity::open(&transport_key)?.id());
            Ok(())
        }
    }
}

fn serve(mut signer: ValidatorSigner, peer: Endpoint, identity: SignerIdentity) -> Result<()> {
    let address = account::Id::from(signer.public_key());
    ensure!(
        address.as_bytes() != identity.id().as_bytes(),
        "use a separate Ed25519 key for the signer transport"
    );
    println!(
        "Tendermint 0.34 signer ready for {} on {}",
        address,
        signer.chain_id()
    );
    loop {
        match SignerConnection::connect(peer, &identity) {
            Ok(mut session) => {
                println!("Tendermint 0.34 signer connected to {}", peer.id);
                let error = loop {
                    let request = match session.receive() {
                        Ok(request) => request,
                        Err(error) => break error,
                    };
                    let reply = match respond(&mut signer, request) {
                        Ok(reply) => reply,
                        Err(error)
                            if !matches!(error.downcast_ref(), Some(SignError::Storage(_))) =>
                        {
                            break error;
                        }
                        Err(error) => return Err(error),
                    };
                    if let Err(error) = session.send(reply) {
                        break error;
                    }
                };
                eprintln!("private validator connection ended: {error:#}");
            }
            Err(error) => eprintln!("private validator connection: {error:#}"),
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn respond(signer: &mut ValidatorSigner, message: privval::Message) -> Result<privval::Message> {
    let chain = |chain_id: &str| {
        (signer.chain_id().as_str() == chain_id)
            .then_some(())
            .context("wrong chain ID")
            .map_err(SignError::Invalid)
    };
    let response = match message.sum.context("empty private validator request")? {
        Sum::PubKeyRequest(request) => {
            let (pub_key, error) =
                reply(chain(&request.chain_id).map(|_| signer.public_key().into()))?;
            Sum::PubKeyResponse(privval::PubKeyResponse { pub_key, error })
        }
        Sum::SignVoteRequest(request) => {
            let result = chain(&request.chain_id).and_then(|_| {
                let mut vote = request
                    .vote
                    .context("missing vote")
                    .map_err(SignError::Invalid)?;
                signer.sign_vote(&mut vote)?;
                Ok(vote)
            });
            let (vote, error) = reply(result)?;
            Sum::SignedVoteResponse(privval::SignedVoteResponse { vote, error })
        }
        Sum::SignProposalRequest(request) => {
            let result = chain(&request.chain_id).and_then(|_| {
                let mut proposal = request
                    .proposal
                    .context("missing proposal")
                    .map_err(SignError::Invalid)?;
                signer.sign_proposal(&mut proposal)?;
                Ok(proposal)
            });
            let (proposal, error) = reply(result)?;
            Sum::SignedProposalResponse(privval::SignedProposalResponse { proposal, error })
        }
        Sum::PingRequest(_) => Sum::PingResponse(privval::PingResponse {}),
        _ => anyhow::bail!("unexpected private validator message"),
    };
    Ok(privval::Message {
        sum: Some(response),
    })
}

fn reply<T>(
    result: Result<T, SignError>,
) -> Result<(Option<T>, Option<privval::RemoteSignerError>)> {
    match result {
        Ok(value) => Ok((Some(value), None)),
        Err(error @ SignError::Storage(_)) => Err(error.into()),
        Err(error) => Ok((
            None,
            Some(privval::RemoteSignerError {
                code: 0,
                description: error.to_string(),
            }),
        )),
    }
}
