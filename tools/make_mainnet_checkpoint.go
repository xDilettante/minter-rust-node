// Run from the pinned Minter Go module with go run -mod=readonly.
package main

import (
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/csv"
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
	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/libs/log"
)

type checkpoint struct {
	Height         int64  `json:"height"`
	AppHash        string `json:"appHash"`
	BlockHash      string `json:"blockHash"`
	Network        string `json:"network"`
	SnapshotSHA256 string `json:"snapshotSha256"`
	Bytes          int64  `json:"bytes"`
	Nodes          uint64 `json:"nodes"`
}

type fileHash struct {
	Bytes int64  `json:"bytes"`
	SHA   string `json:"sha256"`
}

type exchange struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type record struct {
	Height     int64      `json:"height"`
	BlockHash  string     `json:"block_hash"`
	AppHash    string     `json:"app_hash"`
	BeginBlock exchange   `json:"begin_block"`
	DeliverTx  []exchange `json:"deliver_tx"`
	EndBlock   exchange   `json:"end_block"`
	Commit     []byte     `json:"commit_response"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func require(ok bool, msg string, args ...any) {
	if !ok {
		panic(fmt.Sprintf(msg, args...))
	}
}

func readJSON(path string, value any) {
	raw, err := os.ReadFile(path)
	check(err)
	check(json.Unmarshal(raw, value))
}

func verifyFile(path string, expected fileHash) {
	file, err := os.Open(path)
	check(err)
	defer file.Close()
	hash := sha256.New()
	count, err := io.Copy(hash, file)
	check(err)
	require(count == expected.Bytes && hex.EncodeToString(hash.Sum(nil)) == expected.SHA,
		"file differs from corpus manifest: %s", path)
}

func openGzip(path string) (*os.File, *gzip.Reader) {
	file, err := os.Open(path)
	check(err)
	stream, err := gzip.NewReader(file)
	check(err)
	return file, stream
}

func index(path string, last int64) map[int64][]string {
	file, stream := openGzip(path)
	defer file.Close()
	defer stream.Close()
	csv := csv.NewReader(stream)
	columns, err := csv.Read()
	check(err)
	require(len(columns) >= 3 && columns[0] == "height" && columns[1] == "block_hash" &&
		columns[2] == "header_app_hash", "invalid header index")
	rows := make(map[int64][]string)
	for {
		row, err := csv.Read()
		if err == io.EOF {
			break
		}
		check(err)
		height, err := strconv.ParseInt(row[0], 10, 64)
		check(err)
		if height > last {
			break
		}
		rows[height] = row
	}
	return rows
}

func execute(app *minter.Blockchain, block record, previous string) {
	same := func(name string, actual, expected []byte, err error) {
		check(err)
		require(bytes.Equal(actual, expected), "Go %s differs at height %d", name, block.Height)
	}
	var begin abci.RequestBeginBlock
	check(begin.Unmarshal(block.BeginBlock.Request))
	require(begin.Header.Height == block.Height && begin.Header.ChainID == "minter-mainnet-5" &&
		hex.EncodeToString(begin.Header.AppHash) == previous && hex.EncodeToString(begin.Hash) == block.BlockHash,
		"BeginBlock header differs at %d", block.Height)
	beginRes := app.BeginBlock(begin)
	raw, err := beginRes.Marshal()
	same("BeginBlock", raw, block.BeginBlock.Response, err)
	for _, tx := range block.DeliverTx {
		var req abci.RequestDeliverTx
		check(req.Unmarshal(tx.Request))
		result := app.DeliverTx(req)
		raw, err := result.Marshal()
		same("DeliverTx", raw, tx.Response, err)
	}
	var end abci.RequestEndBlock
	check(end.Unmarshal(block.EndBlock.Request))
	endRes := app.EndBlock(end)
	raw, err = endRes.Marshal()
	same("EndBlock", raw, block.EndBlock.Response, err)
	res := app.Commit()
	raw, err = res.Marshal()
	same("Commit", raw, block.Commit, err)
	require(hex.EncodeToString(res.Data) == block.AppHash, "Go root differs at %d", block.Height)
}

func main() {
	if len(os.Args) == 5 && os.Args[1] == "--verify" {
		verifyExport(os.Args[2], os.Args[3], os.Args[4])
		return
	}
	if len(os.Args) != 8 {
		panic("usage: go run make_mainnet_checkpoint.go CORPUS_MANIFEST SNAPSHOT CHECKPOINT_MANIFEST TRACE HEADERS TARGET_BLOCK NEW_OUTPUT_DIRECTORY")
	}
	corpusPath, snapshot, manifest, trace, headers, targetText, out := os.Args[1], os.Args[2],
		os.Args[3], os.Args[4], os.Args[5], os.Args[6], os.Args[7]
	var corpus struct {
		Revision string              `json:"go_oracle_revision"`
		Files    map[string]fileHash `json:"files"`
	}
	readJSON(corpusPath, &corpus)
	verifyFile(snapshot, corpus.Files["go_checkpoint/snapshot.zlib"])
	verifyFile(manifest, corpus.Files["go_checkpoint/manifest.json"])
	verifyFile(trace, corpus.Files["abci-iavl.jsonl.gz"])
	verifyFile(headers, corpus.Files["blocks.csv.gz"])
	var initial checkpoint
	readJSON(manifest, &initial)
	target, err := strconv.ParseInt(targetText, 10, 64)
	check(err)
	require(target > initial.Height+1, "target must be after the initial checkpoint")
	_, err = minter.CheckpointDirectory(out)
	check(err)
	byHeight := index(headers, target+1)
	require(byHeight[initial.Height][1] == initial.BlockHash && byHeight[initial.Height+1][2] == initial.AppHash,
		"initial checkpoint differs from verified headers")

	home, err := os.MkdirTemp("", "minter-mainnet-checkpoint-")
	check(err)
	defer os.Remove(home)
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	cfg.KeepLastStates = 2
	logger := log.NewNopLogger()
	newApp := func() *minter.Blockchain {
		return minter.NewMinterBlockchain(utils.NewStorage(home, ""), cfg, context.Background(), 0, 0, logger)
	}
	app := newApp()
	root, err := app.RestoreCheckpointFile(snapshot, uint64(initial.Height))
	check(err)
	require(hex.EncodeToString(root) == initial.AppHash, "Go import differs from the verified checkpoint")
	file, stream := openGzip(trace)
	defer file.Close()
	defer stream.Close()
	decoder := json.NewDecoder(stream)
	previous := initial.AppHash
	var last record
	for height := initial.Height + 1; height <= target; height++ {
		var block record
		check(decoder.Decode(&block))
		require(block.Height == height && block.BlockHash == byHeight[height][1] && block.AppHash == byHeight[height+1][2],
			"Go trace differs from verified headers at height %d", height)
		if height == target-1 {
			check(app.ConfigureCheckpointExport(out))
		}
		execute(app, block, previous)
		previous = block.AppHash
		last = block
	}
	check(app.Close())
	app = newApp()
	defer app.Close()
	var exported checkpoint
	readJSON(filepath.Join(out, "manifest.json"), &exported)
	newSnapshot := filepath.Join(out, "snapshot.zlib")
	verifyFile(newSnapshot, fileHash{Bytes: exported.Bytes, SHA: exported.SnapshotSHA256})
	root, err = app.RestoreCheckpointFile(newSnapshot, uint64(target-1))
	check(err)
	require(hex.EncodeToString(root) == byHeight[target][2] && exported.AppHash == byHeight[target][2] &&
		exported.Height == target-1 && exported.BlockHash == byHeight[target-1][1],
		"Go checkpoint differs from the verified target header")
	reference := map[string]any{
		"goOracleRevision": corpus.Revision, "verifiedBlocks": target - initial.Height,
		"checkpoint": exported, "targetHeight": target, "targetAppHash": last.AppHash,
		"targetDeliverTxCount": len(last.DeliverTx),
	}
	if len(last.DeliverTx) == 1 {
		addSendQuote(app, last.DeliverTx[0], reference)
	}
	execute(app, last, exported.AppHash)
	evidence, err := json.MarshalIndent(reference, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(out, "reference.json"), append(evidence, '\n'), 0644))
	fmt.Println(string(evidence))
}

func verifyExport(directory, trace, through string) {
	var manifest checkpoint
	readJSON(filepath.Join(directory, "manifest.json"), &manifest)
	snapshot := filepath.Join(directory, "snapshot.zlib")
	verifyFile(snapshot, fileHash{Bytes: manifest.Bytes, SHA: manifest.SnapshotSHA256})
	target, err := strconv.ParseInt(through, 10, 64)
	check(err)
	require(manifest.Network == "minter-mainnet-5" && manifest.Height > 0 && target > manifest.Height,
		"invalid exported mainnet checkpoint or target")

	home, err := os.MkdirTemp("", "minter-verify-export-")
	check(err)
	defer os.RemoveAll(home)
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	cfg.KeepLastStates = 2
	app := minter.NewMinterBlockchain(utils.NewStorage(home, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	defer app.Close()
	root, err := app.RestoreCheckpointFile(snapshot, uint64(manifest.Height))
	check(err)
	require(hex.EncodeToString(root) == manifest.AppHash, "Go recomputed a different snapshot root")

	file, stream := openGzip(trace)
	defer file.Close()
	defer stream.Close()
	decoder := json.NewDecoder(stream)
	previous := manifest.AppHash
	height, transactions, updates := manifest.Height, 0, 0
	for height < target {
		var block record
		check(decoder.Decode(&block))
		if block.Height <= manifest.Height {
			continue
		}
		require(block.Height == height+1, "missing Go reference block %d", height+1)
		execute(app, block, previous)
		height, previous = block.Height, block.AppHash
		transactions += len(block.DeliverTx)
		var end abci.ResponseEndBlock
		check(end.Unmarshal(block.EndBlock.Response))
		updates += len(end.ValidatorUpdates)
	}
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{
		"checkpointHeight": manifest.Height, "checkpointAppHash": manifest.AppHash,
		"blocks": height - manifest.Height, "transactions": transactions, "validatorUpdates": updates,
		"through": height, "appHash": previous,
	}))
}

func addSendQuote(app *minter.Blockchain, pair exchange, reference map[string]any) {
	var raw abci.RequestDeliverTx
	check(raw.Unmarshal(pair.Request))
	tx, err := minter.GetExecutor(minter.V330).DecodeFromBytes(raw.Tx)
	if err != nil || tx.Type != transaction.TypeSend || tx.GasCoin != 0 {
		return
	}
	var delivery abci.ResponseDeliverTx
	check(delivery.Unmarshal(pair.Response))
	commissions := app.CurrentState().Commission().GetCommissions()
	if delivery.Code != 0 || commissions.Coin == 0 {
		return
	}
	quoted := tx.MulGasPrice(tx.Price(commissions))
	if quoted.Sign() != 1 {
		return
	}
	pool := app.CurrentState().Swap().GetSwapper(commissions.Coin, 0)
	r0, r1 := pool.Reserves()
	price, orders := pool.CalculateBuyForSellWithOrders(quoted)
	for _, event := range delivery.Events {
		for _, tag := range event.Attributes {
			if string(tag.Key) == "tx.commission_in_base_coin" {
				require(string(tag.Value) == price.String(), "checkpoint quote differs from signed block fee")
				reference["commissionCoin"] = commissions.Coin
				reference["commissionInput"] = quoted.String()
				reference["reserveCommissionCoin"] = r0.String()
				reference["reserveBip"] = r1.String()
				reference["commissionInBip"] = price.String()
				reference["ordersUsed"] = len(orders)
				return
			}
		}
	}
	panic("missing Send fee tag")
}
