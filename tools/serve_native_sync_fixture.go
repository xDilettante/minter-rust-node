// Run with go run from the pinned Go module. Serve a real Minter snapshot
// and historical blocks through the upstream Tendermint 0.34 reactors.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"time"

	snapshottypes "github.com/cosmos/cosmos-sdk/snapshots/types"
	abci "github.com/tendermint/tendermint/abci/types"
	blockchain "github.com/tendermint/tendermint/blockchain/v0"
	"github.com/tendermint/tendermint/config"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/libs/protoio"
	"github.com/tendermint/tendermint/p2p"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	tmstate "github.com/tendermint/tendermint/proto/tendermint/state"
	"github.com/tendermint/tendermint/proxy"
	"github.com/tendermint/tendermint/state"
	"github.com/tendermint/tendermint/statesync"
	"github.com/tendermint/tendermint/store"
	tm "github.com/tendermint/tendermint/types"
	"github.com/tendermint/tendermint/version"
	db "github.com/tendermint/tm-db"
)

const chunkSize = 10_000_000

type fixture struct {
	abci.BaseApplication
	snapshot abci.Snapshot
	chunks   [][]byte
	mode     string
}

func (f *fixture) ListSnapshots(abci.RequestListSnapshots) abci.ResponseListSnapshots {
	fmt.Println("snapshots requested")
	return abci.ResponseListSnapshots{Snapshots: []*abci.Snapshot{&f.snapshot}}
}

func (f *fixture) LoadSnapshotChunk(req abci.RequestLoadSnapshotChunk) abci.ResponseLoadSnapshotChunk {
	if req.Height != f.snapshot.Height || req.Format != f.snapshot.Format || req.Chunk >= f.snapshot.Chunks {
		return abci.ResponseLoadSnapshotChunk{}
	}
	chunk := f.chunks[req.Chunk]
	fmt.Printf("chunk %d bytes %d\n", req.Chunk, len(chunk))
	if f.mode == "slow" && req.Chunk == 1 {
		time.Sleep(30 * time.Second)
	}
	if f.mode == "corrupt" && req.Chunk == 1 {
		chunk = append([]byte(nil), chunk...)
		chunk[0] ^= 1
	}
	return abci.ResponseLoadSnapshotChunk{Chunk: chunk}
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func read(path string) []byte {
	data, err := os.ReadFile(path)
	must(err)
	return data
}

func main() {
	if len(os.Args) != 5 {
		panic("usage: serve_native_sync_fixture CHECKPOINT BLOCKS LOOPBACK-ADDRESS MODE")
	}
	directory, blocksPath, address, mode := os.Args[1], os.Args[2], os.Args[3], os.Args[4]
	host, _, err := net.SplitHostPort(address)
	must(err)
	if !net.ParseIP(host).IsLoopback() || (mode != "valid" && mode != "corrupt" && mode != "slow") {
		panic("use a loopback address and valid, corrupt or slow mode")
	}
	var manifest struct {
		Network        string
		Height         uint64
		Format         uint32
		SnapshotSHA256 string `json:"snapshotSha256"`
	}
	must(json.Unmarshal(read(filepath.Join(directory, "manifest.json")), &manifest))
	hash, err := hex.DecodeString(manifest.SnapshotSHA256)
	must(err)
	raw := read(filepath.Join(directory, "snapshot.zlib"))
	if fmt.Sprintf("%x", sha256.Sum256(raw)) != manifest.SnapshotSHA256 {
		panic("snapshot hash differs from manifest")
	}
	snapshot := snapshottypes.Snapshot{Height: manifest.Height, Format: manifest.Format, Hash: hash}
	app := &fixture{mode: mode}
	for len(raw) > 0 {
		length := len(raw)
		if length > chunkSize {
			length = chunkSize
		}
		chunk := raw[:length]
		app.chunks = append(app.chunks, chunk)
		digest := sha256.Sum256(chunk)
		snapshot.Metadata.ChunkHashes = append(snapshot.Metadata.ChunkHashes, digest[:])
		raw = raw[length:]
	}
	snapshot.Chunks = uint32(len(app.chunks))
	app.snapshot, err = snapshot.ToABCI()
	must(err)
	connections := proxy.NewAppConns(proxy.NewLocalClientCreator(app))
	must(connections.Start())
	defer connections.Stop()

	blockStore := store.NewBlockStore(db.NewMemDB())
	file, err := os.Open(blocksPath)
	must(err)
	reader := protoio.NewDelimitedReader(file, tm.MaxBlockSizeBytes+1024)
	var previous *tm.Block
	for {
		var message bc.Message
		_, err := reader.ReadMsg(&message)
		if err == io.EOF {
			break
		}
		must(err)
		response := message.GetBlockResponse()
		if response == nil {
			panic("expected BlockResponse")
		}
		block, err := tm.BlockFromProto(response.Block)
		must(err)
		if previous != nil {
			blockStore.SaveBlock(previous, previous.MakePartSet(tm.BlockPartSizeBytes), block.LastCommit)
		}
		previous = block
	}
	must(file.Close())
	if blockStore.Base() == 0 || uint64(blockStore.Height()) < manifest.Height+3 {
		panic("fixture is missing signed successor blocks")
	}
	cfg := config.DefaultConfig()
	key := p2p.NodeKey{PrivKey: ed25519.GenPrivKey()}
	info := p2p.DefaultNodeInfo{
		ProtocolVersion: p2p.NewProtocolVersion(version.P2PProtocol, version.BlockProtocol, 8),
		DefaultNodeID:   key.ID(), ListenAddr: address, Network: manifest.Network,
		Version: version.TMCoreSemVer, Moniker: "minter-fixture",
		Other: p2p.DefaultNodeInfoOther{TxIndex: "off"},
	}
	transport := p2p.NewMultiplexTransport(info, key, p2p.MConnConfig(cfg.P2P))
	sw := p2p.NewSwitch(cfg.P2P, transport)
	sw.SetNodeKey(&key)
	var rawState tmstate.State
	must(rawState.Unmarshal(read(filepath.Join(directory, "state.pb"))))
	consensus, err := state.FromProto(&rawState)
	must(err)
	consensus.LastBlockHeight = blockStore.Height()
	sw.AddReactor("BLOCKCHAIN", blockchain.NewBlockchainReactor(*consensus, nil, blockStore, false))
	sw.AddReactor("STATESYNC", statesync.NewReactor(*cfg.StateSync, connections.Snapshot(), connections.Query(), ""))
	for _, channel := range []byte{blockchain.BlockchainChannel, statesync.SnapshotChannel, statesync.ChunkChannel} {
		info.Channels = append(info.Channels, channel)
		transport.AddChannel(channel)
	}
	sw.SetNodeInfo(info)
	endpoint, err := p2p.NewNetAddressString(fmt.Sprintf("%s@%s", key.ID(), address))
	must(err)
	must(transport.Listen(*endpoint))
	defer transport.Close()
	must(sw.Start())
	defer sw.Stop()
	fmt.Printf("ready %s\n", endpoint)
	select {}
}
