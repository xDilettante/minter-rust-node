// Run from the pinned Minter Go module. Use the signed Go replay from genesis,
// or the validator sets from an authenticated archive for a later snapshot.
package main

import (
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/libs/protoio"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	tmstate "github.com/tendermint/tendermint/proto/tendermint/state"
	tmproto "github.com/tendermint/tendermint/proto/tendermint/types"
	sm "github.com/tendermint/tendermint/state"
	tm "github.com/tendermint/tendermint/types"
	"github.com/tendermint/tendermint/version"
)

const genesisHash = "906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5"

type checkpointManifest struct {
	Network        string
	Height         int64
	BlockHash      string
	AppHash        string
	SnapshotSHA256 string `json:"snapshotSha256"`
	Bytes          int64
}

type replayBlock struct {
	Height    int64
	BlockHash string `json:"block_hash"`
	AppHash   string `json:"app_hash"`
	Begin     struct {
		Request []byte
	} `json:"begin_block"`
	End struct {
		Response []byte
	} `json:"end_block"`
}

type archivedSet struct {
	Validators []struct {
		PublicKey string `json:"public_key"`
		Power     string `json:"voting_power"`
	}
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func require(ok bool, message string, args ...any) {
	if !ok {
		panic(fmt.Sprintf(message, args...))
	}
}

func read(path string) []byte {
	raw, err := os.ReadFile(path)
	must(err)
	return raw
}

func sha(data []byte) string {
	return fmt.Sprintf("%x", sha256.Sum256(data))
}

func blockID(block *tm.Block) tm.BlockID {
	return tm.BlockID{Hash: block.Hash(), PartSetHeader: block.MakePartSet(tm.BlockPartSizeBytes).Header()}
}

func readBlocks(dir string, height int64) ([3]*tm.Block, []byte) {
	var blocks [3]*tm.Block
	var encoded bytes.Buffer
	paths, err := filepath.Glob(filepath.Join(dir, "*.pb"))
	must(err)
	sort.Strings(paths)
	target := height
	for _, path := range paths {
		stem := strings.TrimSuffix(filepath.Base(path), ".pb")
		firstText, lastText, ok := strings.Cut(stem, "-")
		require(ok, "invalid shard name %s", path)
		first, err := strconv.ParseInt(firstText, 10, 64)
		must(err)
		last, err := strconv.ParseInt(lastText, 10, 64)
		must(err)
		if target > last {
			continue
		}
		require(first <= target, "missing block shard at %d", target)
		file, err := os.Open(path)
		must(err)
		reader := protoio.NewDelimitedReader(file, tm.MaxBlockSizeBytes+1024)
		for h := first; h <= last && target <= height+2; h++ {
			var message bc.Message
			_, err := reader.ReadMsg(&message)
			must(err)
			if h < target {
				continue
			}
			response := message.GetBlockResponse()
			require(response != nil, "expected BlockResponse at %d", h)
			block, err := tm.BlockFromProto(response.Block)
			must(err)
			require(block.Height == h, "wrong block height %d", h)
			blocks[target-height] = block
			_, err = protoio.NewDelimitedWriter(&encoded).WriteMsg(&message)
			must(err)
			target++
		}
		must(file.Close())
		if target == height+3 {
			return blocks, encoded.Bytes()
		}
	}
	panic(fmt.Sprintf("missing checkpoint block at %d", target))
}

func readArchivedSet(sets map[string]archivedSet, hash []byte) *tm.ValidatorSet {
	entry, ok := sets[hex.EncodeToString(hash)]
	require(ok && len(entry.Validators) > 0, "missing validator set %x", hash)
	updates := make([]abci.ValidatorUpdate, 0, len(entry.Validators))
	for _, validator := range entry.Validators {
		require(strings.HasPrefix(validator.PublicKey, "Mp"), "invalid Minter validator key")
		key, err := hex.DecodeString(validator.PublicKey[2:])
		must(err)
		require(len(key) == 32, "invalid Ed25519 validator key")
		power, err := strconv.ParseInt(validator.Power, 10, 64)
		must(err)
		require(power > 0, "invalid voting power")
		updates = append(updates, abci.Ed25519ValidatorUpdate(key, power))
	}
	decoded, err := tm.PB2TM.ValidatorUpdates(updates)
	must(err)
	set := tm.NewValidatorSet(decoded)
	require(bytes.Equal(set.Hash(), hash), "validator set hash differs")
	return set
}

func readSnapshot(source string, chain string, initialHeight int64) (checkpointManifest, []byte, []byte) {
	manifestBytes := read(filepath.Join(source, "manifest.json"))
	var manifest checkpointManifest
	must(json.Unmarshal(manifestBytes, &manifest))
	require(manifest.Network == chain && manifest.Height >= initialHeight, "invalid snapshot manifest")
	snapshot := read(filepath.Join(source, "snapshot.zlib"))
	require(int64(len(snapshot)) == manifest.Bytes && sha(snapshot) == manifest.SnapshotSHA256,
		"snapshot differs from manifest")
	return manifest, manifestBytes, snapshot
}

func main() {
	if len(os.Args) == 5 && os.Args[1] == "--priorities" {
		tracePriorities(os.Args[2], os.Args[3], os.Args[4])
		return
	}
	if len(os.Args) == 7 && os.Args[1] == "--archive" {
		exportArchive(os.Args[2], os.Args[3], os.Args[4], os.Args[5], os.Args[6])
		return
	}
	require(len(os.Args) == 7,
		"usage: export GENESIS INIT-RESPONSE GO-REPLAY BLOCK-SHARDS GO-SNAPSHOT NEW-OUTPUT; export --archive GENESIS VALIDATOR-SETS BLOCK-SHARDS GO-SNAPSHOT NEW-OUTPUT; export --priorities GENESIS INIT-RESPONSE GO-REPLAY")
	genesis := read(os.Args[1])
	require(sha(genesis) == genesisHash, "wrong Minter genesis")
	doc, err := tm.GenesisDocFromJSON(genesis)
	must(err)
	var init abci.ResponseInitChain
	must(init.Unmarshal(read(os.Args[2])))
	initial, err := tm.PB2TM.ValidatorUpdates(init.Validators)
	must(err)
	require(len(initial) > 0, "InitChain has no validators")
	current := tm.NewValidatorSet(initial)
	next := current.CopyIncrementProposerPriority(1)
	params := tm.UpdateConsensusParams(*doc.ConsensusParams, init.ConsensusParams)
	lastValidatorsChanged, lastParamsChanged := doc.InitialHeight, doc.InitialHeight

	manifest, manifestBytes, snapshot := readSnapshot(os.Args[5], doc.ChainID, doc.InitialHeight)
	reference, err := os.Open(os.Args[3])
	must(err)
	defer reference.Close()
	trace, err := gzip.NewReader(reference)
	must(err)
	defer trace.Close()
	decoder := json.NewDecoder(trace)
	var row replayBlock
	var previous *tm.ValidatorSet
	for h := doc.InitialHeight; h <= manifest.Height; h++ {
		row = replayBlock{}
		must(decoder.Decode(&row))
		require(row.Height == h, "missing Go replay at %d", h)
		var request abci.RequestBeginBlock
		must(request.Unmarshal(row.Begin.Request))
		require(request.Header.Height == h && request.Header.ChainID == doc.ChainID &&
			bytes.Equal(request.Header.ValidatorsHash, current.Hash()) &&
			bytes.Equal(request.Header.NextValidatorsHash, next.Hash()) &&
			bytes.Equal(request.Header.ConsensusHash, tm.HashConsensusParams(params)),
			"Go replay header differs at %d", h)
		future, end := advanceValidators(next, row.End.Response)
		if len(end.ValidatorUpdates) > 0 {
			lastValidatorsChanged = h + 2
		}
		if end.ConsensusParamUpdates != nil {
			params = tm.UpdateConsensusParams(params, end.ConsensusParamUpdates)
			lastParamsChanged = h + 1
		}
		previous, current, next = current, next, future
	}
	require(row.BlockHash == manifest.BlockHash && row.AppHash == manifest.AppHash,
		"snapshot manifest differs from the Go replay")
	blocks, responses := readBlocks(os.Args[4], manifest.Height)
	finish(os.Args[6], doc, manifest, manifestBytes, snapshot, blocks, responses, previous, current, next,
		params, lastValidatorsChanged, lastParamsChanged, true)
}

func advanceValidators(next *tm.ValidatorSet, response []byte) (*tm.ValidatorSet, abci.ResponseEndBlock) {
	var end abci.ResponseEndBlock
	must(end.Unmarshal(response))
	changes, err := tm.PB2TM.ValidatorUpdates(end.ValidatorUpdates)
	must(err)
	future := next.Copy()
	must(future.UpdateWithChangeSet(changes))
	future.IncrementProposerPriority(1)
	return future, end
}

func tracePriorities(genesisPath, initResponsePath, replayPath string) {
	doc, err := tm.GenesisDocFromJSON(read(genesisPath))
	must(err)
	var init abci.ResponseInitChain
	must(init.Unmarshal(read(initResponsePath)))
	initial, err := tm.PB2TM.ValidatorUpdates(init.Validators)
	must(err)
	require(len(initial) > 0, "InitChain has no validators")
	current := tm.NewValidatorSet(initial)
	next := current.CopyIncrementProposerPriority(1)
	file, err := os.Open(replayPath)
	must(err)
	defer file.Close()
	stream, err := gzip.NewReader(file)
	must(err)
	defer stream.Close()
	rows, out := json.NewDecoder(stream), json.NewEncoder(os.Stdout)
	for height := doc.InitialHeight; ; height++ {
		var row replayBlock
		if err := rows.Decode(&row); err == io.EOF {
			return
		} else {
			must(err)
		}
		require(row.Height == height, "missing Go replay at %d", height)
		var begin abci.RequestBeginBlock
		must(begin.Unmarshal(row.Begin.Request))
		require(begin.Header.Height == height && begin.Header.ChainID == doc.ChainID &&
			bytes.Equal(begin.Header.ValidatorsHash, current.Hash()) &&
			bytes.Equal(begin.Header.NextValidatorsHash, next.Hash()), "Go replay header differs at %d", height)
		future, end := advanceValidators(next, row.End.Response)
		var encoded bytes.Buffer
		writer := protoio.NewDelimitedWriter(&encoded)
		for _, set := range []*tm.ValidatorSet{current, next, future} {
			protoSet, err := set.ToProto()
			must(err)
			_, err = writer.WriteMsg(protoSet)
			must(err)
		}
		must(out.Encode(map[string]interface{}{
			"height": height, "proposerStateSha256": sha(encoded.Bytes()),
			"validatorUpdates": len(end.ValidatorUpdates), "appHash": row.AppHash,
		}))
		current, next = next, future
	}
}

func exportArchive(genesisPath, setsPath, shards, source, output string) {
	genesis := read(genesisPath)
	require(sha(genesis) == genesisHash, "wrong Minter genesis")
	doc, err := tm.GenesisDocFromJSON(genesis)
	must(err)
	manifest, manifestBytes, snapshot := readSnapshot(source, doc.ChainID, doc.InitialHeight)
	var sets map[string]archivedSet
	must(json.Unmarshal(read(setsPath), &sets))
	blocks, responses := readBlocks(shards, manifest.Height)
	previous := readArchivedSet(sets, blocks[0].ValidatorsHash)
	current := readArchivedSet(sets, blocks[1].ValidatorsHash)
	next := readArchivedSet(sets, blocks[1].NextValidatorsHash)
	params := *doc.ConsensusParams
	// The signed header anchors the block limits. Other parameters come from
	// the trusted genesis for this historical Minter corpus.
	params.Version.AppVersion = blocks[1].Version.App
	finish(output, doc, manifest, manifestBytes, snapshot, blocks, responses, previous, current, next, params,
		manifest.Height+2, manifest.Height+1, false)
}

func finish(out string, doc *tm.GenesisDoc, manifest checkpointManifest, manifestBytes, snapshot []byte,
	blocks [3]*tm.Block, responses []byte, previous, current, next *tm.ValidatorSet, params tmproto.ConsensusParams,
	lastValidatorsChanged, lastParamsChanged int64, proposerPriorities bool) {
	last, first, witness := blocks[0], blocks[1], blocks[2]
	id := blockID(last)
	firstID := blockID(first)
	require(last.ChainID == doc.ChainID && first.ChainID == doc.ChainID && witness.ChainID == doc.ChainID,
		"checkpoint blocks are from another chain")
	require(hex.EncodeToString(id.Hash) == manifest.BlockHash &&
		hex.EncodeToString(first.AppHash) == manifest.AppHash, "checkpoint block or AppHash differs")
	require(bytes.Equal(last.ValidatorsHash, previous.Hash()) &&
		bytes.Equal(first.ValidatorsHash, current.Hash()) &&
		bytes.Equal(first.NextValidatorsHash, next.Hash()) &&
		bytes.Equal(witness.ValidatorsHash, next.Hash()), "checkpoint validator hashes differ")
	require(bytes.Equal(first.ConsensusHash, tm.HashConsensusParams(params)), "checkpoint consensus parameters differ")
	require(id.Equals(first.LastBlockID) && firstID.Equals(witness.LastBlockID), "broken checkpoint block link")
	must(previous.VerifyCommit(doc.ChainID, id, last.Height, first.LastCommit))
	must(current.VerifyCommit(doc.ChainID, firstID, first.Height, witness.LastCommit))
	appHash, err := hex.DecodeString(manifest.AppHash)
	must(err)
	state := sm.State{
		Version: tmstate.Version{Consensus: first.Version, Software: version.TMCoreSemVer},
		ChainID: doc.ChainID, InitialHeight: doc.InitialHeight,
		LastBlockHeight: last.Height, LastBlockID: id, LastBlockTime: last.Time,
		LastValidators: previous, Validators: current, NextValidators: next,
		LastHeightValidatorsChanged: lastValidatorsChanged,
		ConsensusParams:             params, LastHeightConsensusParamsChanged: lastParamsChanged,
		LastResultsHash: first.LastResultsHash, AppHash: appHash,
	}
	pb, err := state.ToProto()
	must(err)
	rawState, err := pb.Marshal()
	must(err)
	var fields map[string]json.RawMessage
	must(json.Unmarshal(manifestBytes, &fields))
	fields["consensusStateSha256"] = json.RawMessage(strconv.Quote(sha(rawState)))
	fields["blocksSha256"] = json.RawMessage(strconv.Quote(sha(responses)))
	delete(fields, "proposerPriorities")
	if proposerPriorities {
		fields["proposerPriorities"] = json.RawMessage("true")
	}
	manifestBytes, err = json.MarshalIndent(fields, "", "  ")
	must(err)
	manifestBytes = append(manifestBytes, '\n')

	must(os.Mkdir(out, 0755))
	must(os.WriteFile(filepath.Join(out, "snapshot.zlib"), snapshot, 0644))
	must(os.WriteFile(filepath.Join(out, "state.pb"), rawState, 0644))
	must(os.WriteFile(filepath.Join(out, "blocks.pb"), responses, 0644))
	must(os.WriteFile(filepath.Join(out, "manifest.json"), manifestBytes, 0644))
	fmt.Printf("%s height %d, manifest SHA-256 %s\n", doc.ChainID, manifest.Height, sha(manifestBytes))
	fmt.Printf("State SHA-256 %s, blocks SHA-256 %s\n", sha(rawState), sha(responses))
}
