// Run from the pinned Minter Go module with the IAVL MutationHook enabled.
package main

import (
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/cosmos/iavl"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/libs/log"
	tm "github.com/tendermint/tendermint/types"
)

const genesisSHA = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5"

type mutation struct {
	Key    string `json:"key"`
	Value  string `json:"value,omitempty"`
	Remove bool   `json:"remove,omitempty"`
}

type exchange struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type block struct {
	Height   int64             `json:"height"`
	Begin    exchange          `json:"begin_block"`
	End      exchange          `json:"end_block"`
	Commit   []byte            `json:"commit_response"`
	AppHash  string            `json:"app_hash"`
	Balances map[uint32]string `json:"zero_address_balances"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func encoded(value interface{ Marshal() ([]byte, error) }) []byte {
	data, err := value.Marshal()
	check(err)
	return data
}

func writeJSON(path string, value interface{}) {
	data, err := json.MarshalIndent(value, "", "  ")
	check(err)
	check(os.WriteFile(path, append(data, '\n'), 0644))
}

func main() {
	if len(os.Args) != 3 {
		panic("usage: go run make_official_genesis_reference.go GENESIS NEW_OUTPUT_DIRECTORY")
	}
	raw, err := os.ReadFile(os.Args[1])
	check(err)
	if fmt.Sprintf("%x", sha256.Sum256(raw)) != genesisSHA {
		panic("unexpected official genesis SHA-256")
	}
	doc, err := tm.GenesisDocFromJSON(raw)
	check(err)
	output := os.Args[2]
	check(os.Mkdir(output, 0755))

	traceFile, err := os.Create(filepath.Join(output, "go-init-iavl.jsonl.gz"))
	check(err)
	compressed := gzip.NewWriter(traceFile)
	trace := json.NewEncoder(compressed)
	operations := 0
	iavl.MutationHook = func(_ *iavl.MutableTree, key, value []byte) {
		check(trace.Encode(mutation{hex.EncodeToString(key), hex.EncodeToString(value), value == nil}))
		operations++
	}

	cfg := config.GetConfig(output)
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(output, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	started := time.Now()
	init := app.InitChain(abci.RequestInitChain{
		Time: doc.GenesisTime, ChainId: doc.ChainID, AppStateBytes: doc.AppState,
		InitialHeight: doc.InitialHeight, ConsensusParams: tm.TM2PB.ConsensusParams(doc.ConsensusParams),
	})
	tree := app.DeliverState().Tree().GetLastImmutable()
	if tree.Version() != doc.InitialHeight-1 {
		panic("unexpected IAVL genesis height")
	}
	writeJSON(filepath.Join(output, "result.json"), map[string]interface{}{
		"chain": doc.ChainID, "initialHeight": doc.InitialHeight, "stateHeight": tree.Version(),
		"appHash": hex.EncodeToString(tree.Hash()), "operations": operations,
		"initialValidators": len(init.Validators), "seconds": time.Since(started).Seconds(),
	})
	check(os.WriteFile(filepath.Join(output, "init-response.pb"), encoded(&init), 0644))
	iavl.MutationHook = nil
	check(compressed.Close())
	check(traceFile.Close())

	// Local ABCI inputs using the official starting state. There are no
	// signed network blocks in this fixture.
	proposer := ed25519.PubKey(init.Validators[0].PubKey.GetEd25519()).Address()
	var votes []abci.VoteInfo
	for _, v := range init.Validators {
		key := ed25519.PubKey(v.PubKey.GetEd25519())
		votes = append(votes, abci.VoteInfo{
			Validator:       abci.Validator{Address: key.Address(), Power: v.Power},
			SignedLastBlock: true,
		})
	}
	var blocks []block
	var previous []byte
	for height := doc.InitialHeight; height <= doc.InitialHeight+1; height++ {
		header := tm.Header{
			Height: height, ChainID: doc.ChainID, AppHash: previous, ProposerAddress: proposer,
			Time: doc.GenesisTime.Add(time.Duration(height-doc.InitialHeight) * 5 * time.Second),
		}
		begin := abci.RequestBeginBlock{Header: *header.ToProto()}
		if height > doc.InitialHeight {
			begin.LastCommitInfo.Votes = votes
		}
		begun := app.BeginBlock(begin)
		end := abci.RequestEndBlock{Height: height}
		ended := app.EndBlock(end)
		commit := app.Commit()
		previous = commit.Data
		balances := app.CurrentState().Accounts()
		sample := make(map[uint32]string)
		for _, coin := range []mt.CoinID{0, 1, 1993} {
			sample[uint32(coin)] = balances.GetBalance(mt.Address{}, coin).String()
		}
		blocks = append(blocks, block{
			Height: height, Begin: exchange{encoded(&begin), encoded(&begun)},
			End: exchange{encoded(&end), encoded(&ended)}, Commit: encoded(&commit),
			AppHash: hex.EncodeToString(commit.Data), Balances: sample,
		})
	}
	writeJSON(filepath.Join(output, "synthetic-blocks.json"), blocks)
	check(app.Close())
}
