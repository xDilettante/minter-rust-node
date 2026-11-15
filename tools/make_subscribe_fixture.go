// Run from the pinned Minter Go module.
package main

import (
	"encoding/json"
	"os"
	"time"

	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/crypto/secp256k1"
	tmjson "github.com/tendermint/tendermint/libs/json"
	proto "github.com/tendermint/tendermint/proto/tendermint/types"
	version "github.com/tendermint/tendermint/proto/tendermint/version"
	ctypes "github.com/tendermint/tendermint/rpc/core/types"
	"github.com/tendermint/tendermint/types"
)

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: go run make_subscribe_fixture.go OUTPUT")
	}
	when := time.Date(2025, 1, 2, 3, 4, 5, 123000000, time.UTC)
	val := types.NewValidator(ed25519.PubKey(make([]byte, 32)), 60)
	val.ProposerPriority = -17
	next := &types.Validator{PubKey: secp256k1.PubKey(make([]byte, 33)), VotingPower: 9}
	blockID := types.BlockID{Hash: []byte{0xab}, PartSetHeader: types.PartSetHeader{Total: 2, Hash: []byte{0x7f}}}
	header := types.Header{
		Version: version.Consensus{Block: 11, App: 8}, ChainID: "minter-fixture", Height: 40559760,
		Time: when, LastBlockID: blockID, AppHash: []byte{0xde, 0xad}, ProposerAddress: val.Address,
	}
	commit := &types.Commit{Height: header.Height - 1, BlockID: blockID, Signatures: []types.CommitSig{
		types.NewCommitSigForBlock([]byte{0x88}, val.Address, when),
	}}
	vote := &types.Vote{
		Type: proto.PrecommitType, Height: header.Height - 1, BlockID: blockID,
		Timestamp: when, ValidatorAddress: val.Address, Signature: []byte{0x99},
	}
	duplicate := &types.DuplicateVoteEvidence{
		VoteA: vote, VoteB: &types.Vote{Height: vote.Height, Round: 3, Timestamp: when},
		TotalVotingPower: 120, ValidatorPower: val.VotingPower, Timestamp: when,
	}
	attack := &types.LightClientAttackEvidence{
		ConflictingBlock: &types.LightBlock{
			SignedHeader: &types.SignedHeader{Header: &header, Commit: commit},
			ValidatorSet: &types.ValidatorSet{Validators: []*types.Validator{val, next}, Proposer: val},
		},
		CommonHeight: vote.Height - 1, ByzantineValidators: []*types.Validator{val},
		TotalVotingPower: 120, Timestamp: when,
	}
	tag := abci.Event{Type: "tags", Attributes: []abci.EventAttribute{
		{Key: []byte("tx.type"), Value: []byte("01"), Index: true}, {},
	}}
	end := abci.ResponseEndBlock{
		ValidatorUpdates: []abci.ValidatorUpdate{abci.Ed25519ValidatorUpdate(make([]byte, 32), 47)},
		ConsensusParamUpdates: &abci.ConsensusParams{
			Block:     &abci.BlockParams{MaxBytes: 10000000, MaxGas: -1},
			Evidence:  &proto.EvidenceParams{MaxAgeNumBlocks: 72, MaxAgeDuration: time.Second, MaxBytes: 420},
			Validator: &proto.ValidatorParams{PubKeyTypes: []string{}},
			Version:   &proto.VersionParams{AppVersion: 8},
		},
		Events: []abci.Event{{Type: "empty-attributes", Attributes: []abci.EventAttribute{}}},
	}
	type example struct {
		event string
		data  any
	}
	examples := []example{
		{types.EventNewBlock, types.EventDataNewBlock{
			Block: &types.Block{Header: header, Data: types.Data{Txs: types.Txs{types.Tx{0x01, 0xa2}}},
				Evidence: types.EvidenceData{Evidence: types.EvidenceList{duplicate, attack}}, LastCommit: commit},
			ResultBeginBlock: abci.ResponseBeginBlock{Events: []abci.Event{tag}}, ResultEndBlock: end,
		}},
		{types.EventNewBlockHeader, types.EventDataNewBlockHeader{
			Header: header, NumTxs: 1, ResultBeginBlock: abci.ResponseBeginBlock{Events: []abci.Event{}}, ResultEndBlock: end,
		}},
		{types.EventTx, types.EventDataTx{TxResult: abci.TxResult{
			Height: header.Height, Tx: []byte{0x01, 0xa2}, Result: abci.ResponseDeliverTx{
				GasWanted: 10, GasUsed: 17, Events: []abci.Event{tag},
			},
		}}},
		{types.EventTx, types.EventDataTx{TxResult: abci.TxResult{
			Height: header.Height, Index: 1, Tx: []byte{0x01, 0xf3}, Result: abci.ResponseDeliverTx{
				Code: 102, Log: "failed", GasWanted: -1, Data: []byte{}, Events: []abci.Event{},
			},
		}}},
		{types.EventNewEvidence, types.EventDataNewEvidence{Evidence: duplicate, Height: header.Height}},
		{types.EventNewEvidence, types.EventDataNewEvidence{Evidence: attack, Height: header.Height}},
		{types.EventValidatorSetUpdates, types.EventDataValidatorSetUpdates{ValidatorUpdates: []*types.Validator{val, next}}},
		{types.EventVote, types.EventDataVote{Vote: vote}},
		{types.EventNewRoundStep, types.EventDataRoundState{Height: header.Height, Round: 1, Step: "RoundStepPrecommit"}},
		{types.EventNewRound, types.EventDataNewRound{Height: header.Height, Proposer: types.ValidatorInfo{Address: val.Address}}},
		{types.EventCompleteProposal, types.EventDataCompleteProposal{Height: header.Height, BlockID: blockID}},
	}
	type sample struct {
		Wire json.RawMessage `json:"wire"`
		Data json.RawMessage `json:"data"`
	}
	result := make([]sample, 0, len(examples))
	for _, row := range examples {
		wire, err := tmjson.Marshal(&ctypes.ResultEvent{
			Query: types.QueryForEvent(row.event).String(), Data: row.data,
			Events: map[string][]string{types.EventTypeKey: {row.event}},
		})
		check(err)
		data, err := json.Marshal(row.data)
		check(err)
		result = append(result, sample{Wire: wire, Data: data})
	}
	// Keep each vector on one line so changes to JSON encodings are easy to diff.
	out, err := os.Create(os.Args[1])
	check(err)
	encoder := json.NewEncoder(out)
	for _, row := range result {
		check(encoder.Encode(row))
	}
	check(out.Close())
}
