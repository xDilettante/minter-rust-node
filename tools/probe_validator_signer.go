// Build from the pinned Minter Go module. Uses the public devnet fixture key.
package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/libs/log"
	"github.com/tendermint/tendermint/privval"
	pb "github.com/tendermint/tendermint/proto/tendermint/types"
	tm "github.com/tendermint/tendermint/types"
)

const chain = "minter-rust-send-fixture"
const height = 123

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func require(ok bool, reason string) {
	if !ok {
		panic(reason)
	}
}

func rejection(err error, reason string) {
	var remote *privval.RemoteSignerError
	require(errors.As(err, &remote) && strings.Contains(remote.Description, reason),
		fmt.Sprintf("expected remote signer rejection %q, received %v", reason, err))
}

func main() {
	require(len(os.Args) == 5, "usage: probe-validator-signer initial|recover ADDRESS FIXTURE OUTPUT")
	mode, address, fixture, output := os.Args[1], os.Args[2], os.Args[3], os.Args[4]
	require(mode == "initial" || mode == "recover", "unknown probe mode")
	keyFile := filepath.Join(fixture, "priv_validator_key.json")
	source := privval.LoadFilePV(keyFile, filepath.Join(fixture, "priv_validator_state.json"))
	local := privval.NewFilePV(source.Key.PrivKey, keyFile, filepath.Join(output, "last-sign.json"))
	listener, err := net.Listen("tcp", address)
	check(err)
	transport := ed25519.GenPrivKeyFromSecret([]byte("public Rust signing probe transport"))
	endpoint := privval.NewSignerListenerEndpoint(log.NewNopLogger(),
		privval.NewTCPListener(listener, transport))
	client, err := privval.NewSignerClient(endpoint, chain)
	check(err)
	defer endpoint.Stop()
	fmt.Println("Go private validator probe listening", hex.EncodeToString(transport.PubKey().Address()))
	check(client.WaitForConnection(10 * time.Second))
	pub, err := client.GetPubKey()
	check(err)
	require(pub.Equals(source.Key.PubKey), "signer uses the wrong public key")
	check(client.Ping())
	timestamp := time.Unix(1735689600, 123456789).UTC()
	block := pb.BlockID{Hash: bytes.Repeat([]byte{3}, 32),
		PartSetHeader: pb.PartSetHeader{Total: 3, Hash: bytes.Repeat([]byte{4}, 32)}}
	vote := pb.Vote{Type: pb.PrevoteType, Height: height, Round: 1, BlockID: block,
		Timestamp: timestamp, ValidatorAddress: pub.Address(), ValidatorIndex: 1}
	expect := func(request pb.Vote) pb.Vote {
		check(local.SignVote(chain, &request))
		return request
	}
	verify := func(actual, expected pb.Vote) {
		require(bytes.Equal(actual.Signature, expected.Signature) &&
			actual.Timestamp.Equal(expected.Timestamp) &&
			pub.VerifySignature(tm.VoteSignBytes(chain, &actual), actual.Signature),
			"vote signature or timestamp differs from Go")
	}

	proposal := pb.Proposal{Type: pb.ProposalType, Height: height, Round: 1, PolRound: 0,
		BlockID: block, Timestamp: timestamp}
	wantProposal := proposal
	check(local.SignProposal(chain, &wantProposal))
	prevote := vote
	prevote.BlockID = pb.BlockID{}
	wantNil := expect(prevote)
	vote.Type = pb.PrecommitType
	wantVote := expect(vote)

	if mode == "initial" {
		check(client.SignProposal(chain, &proposal))
		require(bytes.Equal(proposal.Signature, wantProposal.Signature) &&
			proposal.Timestamp.Equal(wantProposal.Timestamp) &&
			pub.VerifySignature(tm.ProposalSignBytes(chain, &proposal), proposal.Signature),
			"proposal signature differs from Go")
		check(client.SignVote(chain, &prevote))
		verify(prevote, wantNil)
		check(client.SignVote(chain, &vote))
		verify(vote, wantVote)
	} else {
		retry := vote
		retry.Timestamp = timestamp.Add(time.Second)
		check(client.SignVote(chain, &retry))
		verify(retry, wantVote)
		conflict := vote
		conflict.BlockID.Hash = bytes.Repeat([]byte{5}, 32)
		rejection(client.SignVote(chain, &conflict), "conflicting data")
		conflict = vote
		conflict.BlockID.PartSetHeader.Total++
		rejection(client.SignVote(chain, &conflict), "conflicting data")
		conflict = vote
		conflict.BlockID = pb.BlockID{}
		rejection(client.SignVote(chain, &conflict), "conflicting data")
		for _, previous := range []pb.Vote{
			{Type: pb.PrecommitType, Height: height - 1, Round: 2},
			{Type: pb.PrecommitType, Height: height, Round: 0},
			{Type: pb.PrevoteType, Height: height, Round: 1},
		} {
			previous.Timestamp, previous.ValidatorAddress = timestamp, pub.Address()
			rejection(client.SignVote(chain, &previous), "regressed")
		}
		next := vote
		next.Height++
		rejection(client.SignVote("wrong-chain", &next), "wrong chain ID")
		invalid := next
		invalid.ValidatorAddress = bytes.Repeat([]byte{0}, 20)
		rejection(client.SignVote(chain, &invalid), "validator address differs")
		want := expect(next)
		check(client.SignVote(chain, &next))
		verify(next, want)
	}
	check(json.NewEncoder(os.Stdout).Encode(map[string]interface{}{
		"mode": mode, "signedHeight": height, "exactGoSignatures": true,
		"replayedTimestamp": mode == "recover", "rejectedConflicts": mode == "recover",
		"rejectedRegressions": mode == "recover", "rejectedWrongChain": mode == "recover",
		"rejectedWrongAddress": mode == "recover",
	}))
}
