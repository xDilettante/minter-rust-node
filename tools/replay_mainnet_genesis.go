// Run from the pinned Minter Go module with go run -mod=readonly.
package main

import (
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"

	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/libs/log"
	"github.com/tendermint/tendermint/libs/protoio"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	tmstate "github.com/tendermint/tendermint/proto/tendermint/state"
	tmproto "github.com/tendermint/tendermint/proto/tendermint/types"
	sm "github.com/tendermint/tendermint/state"
	tm "github.com/tendermint/tendermint/types"
)

const genesisSHA256 = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5"

type pair struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type trace struct {
	Height     int64  `json:"height"`
	BlockHash  string `json:"block_hash"`
	AppHash    string `json:"app_hash"`
	Begin      pair   `json:"begin_block"`
	Deliveries []pair `json:"deliver_tx"`
	End        pair   `json:"end_block"`
	Commit     []byte `json:"commit_response"`
}

type replayBase struct {
	Format        uint32  `json:"format"`
	Network       string  `json:"network"`
	Height        int64   `json:"height"`
	AppHash       string  `json:"appHash"`
	BlockHash     string  `json:"blockHash"`
	Bytes         int64   `json:"bytes"`
	SnapshotHash  string  `json:"snapshotSha256"`
	StateHash     string  `json:"consensusStateSha256"`
	BlocksHash    string  `json:"blocksSha256"`
	GenesisHash   *string `json:"genesisSha256"`
	TransactionID *uint8  `json:"transactionChainId"`
	state         *sm.State
	manifest      json.RawMessage
	manifestHash  string
	snapshot      string
}

func digestMatches(raw []byte, expected, name string) {
	want, err := hex.DecodeString(expected)
	check(err)
	got := sha256.Sum256(raw)
	require(len(want) == len(got) && bytes.Equal(got[:], want), "%s SHA-256 differs", name)
}

func blockID(block *tm.Block) tm.BlockID {
	return tm.BlockID{Hash: block.Hash(), PartSetHeader: block.MakePartSet(tm.BlockPartSizeBytes).Header()}
}

func loadBase(directory, trusted string, doc *tm.GenesisDoc) *replayBase {
	read := func(name string) []byte {
		raw, err := os.ReadFile(filepath.Join(directory, name))
		check(err)
		return raw
	}
	raw := read("manifest.json")
	digestMatches(raw, trusted, "checkpoint manifest")
	var base replayBase
	check(json.Unmarshal(raw, &base))
	require(base.Format == 1 && base.Network == doc.ChainID && base.Height >= doc.InitialHeight && base.Height <= (1<<63)-3,
		"checkpoint format, network or height differs from genesis")
	require((base.GenesisHash == nil || *base.GenesisHash == genesisSHA256) &&
		(base.TransactionID == nil || *base.TransactionID == 1), "checkpoint genesis identity differs")
	base.manifest = raw
	base.manifestHash = fmt.Sprintf("%x", sha256.Sum256(raw))
	base.snapshot = filepath.Join(directory, "snapshot.zlib")
	snapshot := read("snapshot.zlib")
	require(int64(len(snapshot)) == base.Bytes, "checkpoint snapshot size differs")
	digestMatches(snapshot, base.SnapshotHash, "checkpoint snapshot")
	stateBytes, blocks := read("state.pb"), read("blocks.pb")
	digestMatches(stateBytes, base.StateHash, "checkpoint state")
	digestMatches(blocks, base.BlocksHash, "checkpoint blocks")
	var pb tmstate.State
	check(pb.Unmarshal(stateBytes))
	state, err := sm.FromProto(&pb)
	check(err)
	require(state.ChainID == doc.ChainID && state.InitialHeight == doc.InitialHeight &&
		state.LastBlockHeight == base.Height && hex.EncodeToString(state.AppHash) == base.AppHash &&
		hex.EncodeToString(state.LastBlockID.Hash) == base.BlockHash && state.LastValidators != nil &&
		state.Validators != nil && state.NextValidators != nil, "checkpoint consensus state differs")
	reader := protoio.NewDelimitedReader(bytes.NewReader(blocks), tm.MaxBlockSizeBytes+1024)
	anchor := nextBlock(reader, doc.ChainID, base.Height)
	first := nextBlock(reader, doc.ChainID, base.Height+1)
	witness := nextBlock(reader, doc.ChainID, base.Height+2)
	var extra bc.Message
	count, err := reader.ReadMsg(&extra)
	require(err == io.EOF && count == 0, "checkpoint must contain exactly three anchor blocks")
	require(blockID(anchor).Equals(state.LastBlockID) && anchor.Time.Equal(state.LastBlockTime) &&
		state.Version.Consensus.Block == first.Version.Block && state.Version.Consensus.App == first.Version.App &&
		state.LastBlockID.Equals(first.LastBlockID) &&
		blockID(first).Equals(witness.LastBlockID) && bytes.Equal(state.AppHash, first.AppHash) &&
		bytes.Equal(state.LastResultsHash, first.LastResultsHash) &&
		bytes.Equal(state.LastValidators.Hash(), anchor.ValidatorsHash) &&
		bytes.Equal(state.Validators.Hash(), anchor.NextValidatorsHash) &&
		bytes.Equal(state.Validators.Hash(), first.ValidatorsHash) &&
		bytes.Equal(state.NextValidators.Hash(), first.NextValidatorsHash) &&
		bytes.Equal(state.NextValidators.Hash(), witness.ValidatorsHash) &&
		bytes.Equal(tm.HashConsensusParams(state.ConsensusParams), first.ConsensusHash),
		"checkpoint anchor fields differ")
	check(state.LastValidators.VerifyCommit(doc.ChainID, state.LastBlockID, anchor.Height, first.LastCommit))
	check(state.Validators.VerifyCommit(doc.ChainID, blockID(first), first.Height, witness.LastCommit))
	base.state = state
	return &base
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func require(ok bool, format string, args ...any) {
	if !ok {
		panic(fmt.Sprintf(format, args...))
	}
}

func encode(msg interface{ Marshal() ([]byte, error) }) []byte {
	data, err := msg.Marshal()
	check(err)
	return data
}

func nextBlock(reader protoio.Reader, chain string, height int64) *tm.Block {
	var msg bc.Message
	_, err := reader.ReadMsg(&msg)
	check(err)
	response := msg.GetBlockResponse()
	require(response != nil, "expected BlockResponse at %d", height)
	block, err := tm.BlockFromProto(response.Block)
	check(err)
	require(block.ChainID == chain && block.Height == height, "wrong block at %d", height)
	return block
}

func begin(block *tm.Block, previous *tm.ValidatorSet) abci.RequestBeginBlock {
	request := abci.RequestBeginBlock{Header: *block.Header.ToProto(), Hash: block.Hash()}
	request.LastCommitInfo.Round = block.LastCommit.Round
	if previous != nil {
		require(block.LastCommit.Size() == len(previous.Validators), "wrong previous validator count at %d", block.Height)
		for i, val := range previous.Validators {
			request.LastCommitInfo.Votes = append(request.LastCommitInfo.Votes, abci.VoteInfo{
				Validator:       tm.TM2PB.Validator(val),
				SignedLastBlock: !block.LastCommit.Signatures[i].Absent(),
			})
		}
	}
	for _, evidence := range block.Evidence.Evidence {
		request.ByzantineValidators = append(request.ByzantineValidators, evidence.ABCI()...)
	}
	return request
}

func main() {
	args := os.Args[1:]
	var checkpointDirectory, trustedCheckpoint string
	if len(args) > 0 && args[0] == "--checkpoint" {
		require(len(args) >= 7, "checkpoint replay requires PACKAGE MANIFEST_SHA256 GENESIS BLOCK-SHARDS NEW-OUTPUT LAST-HEIGHT")
		checkpointDirectory, trustedCheckpoint = args[1], args[2]
		require(checkpointDirectory != "", "checkpoint package path is empty")
		args = args[3:]
	}
	require(len(args) >= 4, "usage: replay [--checkpoint PACKAGE MANIFEST_SHA256] GENESIS BLOCK-SHARDS NEW-OUTPUT LAST-HEIGHT [CHECKPOINT-HEIGHT ...]")
	raw, err := os.ReadFile(args[0])
	check(err)
	require(fmt.Sprintf("%x", sha256.Sum256(raw)) == genesisSHA256, "unexpected genesis SHA-256")
	doc, err := tm.GenesisDocFromJSON(raw)
	check(err)
	var base *replayBase
	first := doc.InitialHeight
	if checkpointDirectory != "" {
		base = loadBase(checkpointDirectory, trustedCheckpoint, doc)
		first = base.Height + 1
	}
	through, err := strconv.ParseInt(args[3], 10, 64)
	check(err)
	require(through >= first && through <= (1<<63)-3, "replay range is outside supported heights")
	checkpoints := make(map[int64]bool)
	for _, text := range args[4:] {
		height, err := strconv.ParseInt(text, 10, 64)
		check(err)
		require(first <= height && height <= through, "checkpoint is outside the replay range")
		checkpoints[height] = true
	}
	paths, err := filepath.Glob(filepath.Join(args[1], "*.pb"))
	check(err)
	require(len(paths) > 0, "no P2P block shards")
	files := make([]io.Reader, 0, len(paths))
	for _, path := range paths {
		file, err := os.Open(path)
		check(err)
		defer file.Close()
		files = append(files, file)
	}
	reader := protoio.NewDelimitedReader(io.MultiReader(files...), tm.MaxBlockSizeBytes+1024)
	firstRead := first
	if base != nil {
		_, err := fmt.Sscanf(filepath.Base(paths[0]), "%d-", &firstRead)
		check(err)
		require(firstRead >= doc.InitialHeight && firstRead <= base.Height,
			"raw input does not include checkpoint anchor")
	}
	block := nextBlock(reader, doc.ChainID, firstRead)
	if base == nil {
		require(block.LastCommit.Size() == 0 && bytes.Equal(block.AppHash, doc.AppHash), "initial header differs from genesis")
	} else {
		for block.Height < base.Height {
			block = nextBlock(reader, doc.ChainID, block.Height+1)
		}
		require(blockID(block).Equals(base.state.LastBlockID), "raw checkpoint anchor differs")
		block = nextBlock(reader, doc.ChainID, first)
		require(block.LastBlockID.Equals(base.state.LastBlockID) && bytes.Equal(block.AppHash, base.state.AppHash) &&
			bytes.Equal(block.LastResultsHash, base.state.LastResultsHash), "raw checkpoint successor differs")
	}

	out := args[2]
	check(os.Mkdir(out, 0755))
	cfg := config.GetConfig(filepath.Join(out, "go-home"))
	cfg.DBBackend = "memdb"
	cfg.KeepLastStates = 2
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	var current, nextValidators, previous *tm.ValidatorSet
	if base == nil {
		request := abci.RequestInitChain{Time: doc.GenesisTime, ChainId: doc.ChainID, InitialHeight: doc.InitialHeight, AppStateBytes: doc.AppState}
		response := app.InitChain(request)
		initial, err := tm.PB2TM.ValidatorUpdates(response.Validators)
		check(err)
		current = tm.NewValidatorSet(initial)
		nextValidators = current.Copy()
	} else {
		root, err := app.RestoreCheckpointFile(base.snapshot, uint64(base.Height))
		check(err)
		require(bytes.Equal(root, base.state.AppHash), "restored Go checkpoint root differs")
		previous, current, nextValidators = base.state.LastValidators.Copy(), base.state.Validators.Copy(), base.state.NextValidators.Copy()
	}
	initialRoot := hex.EncodeToString(app.DeliverState().Tree().GetLastImmutable().Hash())
	fmt.Printf("Go replay from %d: %s, %d validators\n", first, initialRoot, current.Size())

	traceFile, err := os.Create(filepath.Join(out, "abci.jsonl.gz.partial"))
	check(err)
	gzipWriter, err := gzip.NewWriterLevel(traceFile, gzip.BestSpeed)
	check(err)
	encoder := json.NewEncoder(gzipWriter)
	var last trace
	var txs, updates int
	for block.Height <= through+1 {
		next := nextBlock(reader, doc.ChainID, block.Height+1)
		require(bytes.Equal(current.Hash(), block.ValidatorsHash), "validator set differs at %d", block.Height)
		require(bytes.Equal(nextValidators.Hash(), block.NextValidatorsHash), "next validator set differs at %d", block.Height)
		id := tm.BlockID{Hash: block.Hash(), PartSetHeader: block.MakePartSet(tm.BlockPartSizeBytes).Header()}
		require(id.Equals(next.LastBlockID), "broken block link at %d", block.Height)
		check(current.VerifyCommit(doc.ChainID, id, block.Height, next.LastCommit))
		if block.Height == first {
			fmt.Printf("Verified first commit at %d for block %X\n", block.Height, id.Hash)
		}
		if block.Height > through {
			break // This signed successor carries the final AppHash and results.
		}

		if checkpoints[block.Height] {
			check(app.ConfigureCheckpointExport(filepath.Join(out, fmt.Sprint(block.Height)+".partial")))
		}
		last = trace{Height: block.Height, BlockHash: hex.EncodeToString(id.Hash)}
		start := begin(block, previous)
		began := app.BeginBlock(start)
		last.Begin = pair{encode(&start), encode(&began)}
		var deliveries []*abci.ResponseDeliverTx
		for _, tx := range block.Txs {
			request := abci.RequestDeliverTx{Tx: tx}
			response := app.DeliverTx(request)
			deliveries = append(deliveries, &response)
			last.Deliveries = append(last.Deliveries, pair{encode(&request), encode(&response)})
		}
		endRequest := abci.RequestEndBlock{Height: block.Height}
		end := app.EndBlock(endRequest)
		last.End = pair{encode(&endRequest), encode(&end)}
		commit := app.Commit()
		last.Commit, last.AppHash = encode(&commit), hex.EncodeToString(commit.Data)
		require(bytes.Equal(commit.Data, next.AppHash), "AppHash differs after %d: Go %X, chain %X", block.Height, commit.Data, next.AppHash)
		require(bytes.Equal(tm.NewResults(deliveries).Hash(), next.LastResultsHash), "transaction results differ at %d", block.Height)
		if params := end.GetConsensusParamUpdates().GetBlock(); params != nil {
			hash := tm.HashConsensusParams(tmproto.ConsensusParams{Block: tmproto.BlockParams{
				MaxBytes: params.MaxBytes, MaxGas: params.MaxGas,
			}})
			require(bytes.Equal(hash, next.ConsensusHash), "consensus parameters differ at %d", block.Height)
		}
		future := nextValidators.Copy()
		changes, err := tm.PB2TM.ValidatorUpdates(end.ValidatorUpdates)
		check(err)
		check(future.UpdateWithChangeSet(changes))
		previous, current, nextValidators = current, nextValidators, future
		txs += len(deliveries)
		updates += len(changes)
		check(encoder.Encode(last))
		if block.Height%1000 == 0 || block.Height == through {
			fmt.Printf("Executed %d, AppHash %s\n", block.Height, last.AppHash)
		}
		block = next
	}
	check(gzipWriter.Close())
	check(traceFile.Close())
	result := map[string]any{
		"chain": doc.ChainID, "genesisSha256": genesisSHA256, "initialAppHash": initialRoot,
		"from": first, "through": through, "signedThrough": through + 1,
		"transactions": txs, "validatorUpdates": updates, "appHash": last.AppHash,
		"versions": app.UpdateVersions(),
	}
	if base != nil {
		result["startingCheckpoint"] = base.manifest
		result["startingCheckpointManifestSha256"] = base.manifestHash
	}
	summary, err := json.MarshalIndent(result, "", "  ")
	check(err)
	check(app.Close())
	for height := range checkpoints {
		check(os.Rename(filepath.Join(out, fmt.Sprint(height)+".partial"), filepath.Join(out, fmt.Sprint(height))))
	}
	check(os.Rename(traceFile.Name(), filepath.Join(out, "abci.jsonl.gz")))
	check(os.WriteFile(filepath.Join(out, "result.json"), append(summary, '\n'), 0644))
	fmt.Println(string(summary))
}
