// Read authentic block responses and validator sets from the Minter corpus.
// Run in the pinned Go Minter module with go run -mod=readonly.
package main

import (
	"bytes"
	"compress/gzip"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"strconv"
	"strings"

	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	tmjson "github.com/tendermint/tendermint/libs/json"
	"github.com/tendermint/tendermint/libs/protoio"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	rpc "github.com/tendermint/tendermint/rpc/core/types"
	tm "github.com/tendermint/tendermint/types"
)

type validatorSample struct {
	Validators []struct {
		PublicKey string `json:"public_key"`
		Power     int64  `json:"voting_power,string"`
	} `json:"validators"`
}

type lightBlock struct {
	SignedHeader   tm.SignedHeader  `json:"signed_header"`
	Validators     *tm.ValidatorSet `json:"validator_set"`
	NextValidators *tm.ValidatorSet `json:"next_validator_set"`
	Provider       string           `json:"provider"`
	Queries        *txQueries       `json:"queries,omitempty"`
}

type txQueries struct {
	Transactions []rpc.ResultTx         `json:"transactions"`
	Results      rpc.ResultBlockResults `json:"results"`
}

type delivery struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func validators(samples map[string]validatorSample, hash []byte) *tm.ValidatorSet {
	sample, ok := samples[hex.EncodeToString(hash)]
	if !ok || len(sample.Validators) == 0 {
		panic(fmt.Sprintf("missing validator set %x", hash))
	}
	list := make([]*tm.Validator, 0, len(sample.Validators))
	for _, item := range sample.Validators {
		key, err := hex.DecodeString(strings.TrimPrefix(item.PublicKey, "Mp"))
		check(err)
		if len(key) != ed25519.PubKeySize || item.Power <= 0 {
			panic("invalid validator")
		}
		list = append(list, tm.NewValidator(ed25519.PubKey(key), item.Power))
	}
	set := tm.NewValidatorSet(list)
	if !bytes.Equal(set.Hash(), hash) {
		panic(fmt.Sprintf("validator set hash differs from %x", hash))
	}
	return set
}

func main() {
	trace := flag.String("trace", "", "optional ABCI trace to export transaction and result responses")
	flag.Parse()
	args := flag.Args()
	if len(args) < 3 {
		panic("usage: go run make_light_fixture.go [-trace FILE] blocks.p2p.pb.gz validator-sets.json HEIGHT...")
	}
	wanted := map[int64]bool{}
	var last int64
	for _, value := range args[2:] {
		height, err := strconv.ParseInt(value, 10, 64)
		check(err)
		wanted[height] = true
		last = max(last, height)
	}
	var deliveries map[int64][]delivery
	if *trace != "" {
		deliveries = readDeliveries(*trace, wanted, last)
	}
	var samples map[string]validatorSample
	raw, err := os.ReadFile(args[1])
	check(err)
	check(json.Unmarshal(raw, &samples))
	file, err := os.Open(args[0])
	check(err)
	defer file.Close()
	compressed, err := gzip.NewReader(file)
	check(err)
	defer compressed.Close()
	input := protoio.NewDelimitedReader(compressed, tm.MaxBlockSizeBytes+1024)
	var previous *tm.Block
	result := make([]lightBlock, 0, len(wanted))
	for len(result) < len(wanted) {
		var message bc.Message
		_, err := input.ReadMsg(&message)
		check(err)
		response := message.GetBlockResponse()
		if response == nil {
			panic("expected Tendermint BlockResponse")
		}
		block, err := tm.BlockFromProto(response.Block)
		check(err)
		if block.ChainID != "minter-mainnet-5" {
			panic("unexpected chain ID")
		}
		if previous != nil && wanted[previous.Height] {
			header := previous.Header
			set := validators(samples, header.ValidatorsHash)
			next := validators(samples, header.NextValidatorsHash)
			if block.Height != header.Height+1 || !bytes.Equal(block.LastBlockID.Hash, previous.Hash()) {
				panic("block successor does not link to the requested header")
			}
			check(set.VerifyCommitLight(header.ChainID, block.LastBlockID, header.Height, block.LastCommit))
			_, set.Proposer = set.GetByAddress(header.ProposerAddress)
			if set.Proposer == nil {
				panic("missing proposer")
			}
			result = append(result, lightBlock{
				SignedHeader: tm.SignedHeader{Header: &header, Commit: block.LastCommit},
				Validators:   set, NextValidators: next,
				Provider: "0101010101010101010101010101010101010101",
				Queries:  queries(previous, block, deliveries[header.Height]),
			})
		}
		previous = block
	}
	encoded, err := tmjson.Marshal(result)
	check(err)
	fmt.Println(string(encoded))
}

func readDeliveries(path string, wanted map[int64]bool, last int64) map[int64][]delivery {
	file, err := os.Open(path)
	check(err)
	defer file.Close()
	input, err := gzip.NewReader(file)
	check(err)
	defer input.Close()
	decoder := json.NewDecoder(input)
	result := map[int64][]delivery{}
	for {
		var row struct {
			Height int64      `json:"height"`
			Txs    []delivery `json:"deliver_tx"`
		}
		check(decoder.Decode(&row))
		if wanted[row.Height] {
			result[row.Height] = row.Txs
		}
		if row.Height >= last {
			break
		}
	}
	return result
}

func queries(block, successor *tm.Block, deliveries []delivery) *txQueries {
	if len(deliveries) == 0 {
		return nil
	}
	if len(deliveries) != len(block.Txs) || !bytes.Equal(block.Txs.Hash(), block.DataHash) {
		panic("recorded transactions differ from the signed block")
	}
	result := &txQueries{Results: rpc.ResultBlockResults{Height: block.Height}}
	for i, delivery := range deliveries {
		var request abci.RequestDeliverTx
		check(request.Unmarshal(delivery.Request))
		if !bytes.Equal(request.Tx, block.Txs[i]) {
			panic("ABCI input differs from the signed transaction")
		}
		var response abci.ResponseDeliverTx
		check(response.Unmarshal(delivery.Response))
		result.Results.TxsResults = append(result.Results.TxsResults, &response)
		result.Transactions = append(result.Transactions, rpc.ResultTx{
			Hash: block.Txs[i].Hash(), Height: block.Height, Index: uint32(i),
			Tx: block.Txs[i], TxResult: response, Proof: block.Txs.Proof(i),
		})
	}
	if !bytes.Equal(tm.NewResults(result.Results.TxsResults).Hash(), successor.LastResultsHash) {
		panic("ABCI results differ from the signed successor")
	}
	return result
}
