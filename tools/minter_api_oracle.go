// Build from the pinned Minter Go module.
package main

import (
	"compress/gzip"
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptrace"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"time"

	"github.com/MinterTeam/minter-go-node/api/v2"
	"github.com/MinterTeam/minter-go-node/api/v2/service"
	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	pb "github.com/MinterTeam/node-grpc-gateway/api_pb"
	"github.com/gorilla/websocket"
	abciserver "github.com/tendermint/tendermint/abci/server"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/libs/log"
	tm "github.com/tendermint/tendermint/node"
	"github.com/tendermint/tendermint/p2p"
	"github.com/tendermint/tendermint/privval"
	"github.com/tendermint/tendermint/proxy"
	"github.com/tendermint/tendermint/rpc/client/local"
	"github.com/tendermint/tendermint/rpc/core"
	rpcserver "github.com/tendermint/tendermint/rpc/jsonrpc/server"
	"github.com/tendermint/tendermint/state/txindex/kv"
	db "github.com/tendermint/tm-db"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/emptypb"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func serve(home, fixture, rpc, peer, grpcAddr, httpAddr string, keep int64) {
	must(os.Mkdir(home, 0755))
	cfg := config.GetConfig(home)
	cfg.DBBackend = "goleveldb"
	cfg.KeepLastStates = keep
	tc := config.GetTmConfig(cfg)
	tc.SetRoot(home)
	tc.DBBackend = "goleveldb"
	tc.RPC.ListenAddress = "tcp://" + rpc
	tc.P2P.ListenAddress = "tcp://" + peer
	tc.P2P.Seeds, tc.P2P.PersistentPeers = "", ""
	tc.P2P.PexReactor = false
	tc.P2P.AddrBookStrict = false
	tc.P2P.AllowDuplicateIP = true
	tc.Consensus.TimeoutCommit = 750 * time.Millisecond

	for name, destination := range map[string]string{
		"genesis.json": tc.GenesisFile(), "priv_validator_key.json": tc.PrivValidatorKeyFile(),
		"priv_validator_state.json": tc.PrivValidatorStateFile(),
	} {
		must(os.MkdirAll(filepath.Dir(destination), 0755))
		content, err := os.ReadFile(filepath.Join(fixture, name))
		must(err)
		must(os.WriteFile(destination, content, 0600))
	}

	storage := utils.NewStorage(home, "")
	_, err := storage.InitStateLevelDB("state", nil)
	must(err)
	logger := log.NewNopLogger()
	app := minter.NewMinterBlockchain(storage, cfg, context.Background(), 0, 0, logger)
	key, err := p2p.LoadOrGenNodeKey(tc.NodeKeyFile())
	must(err)
	node, err := tm.NewNode(tc, privval.LoadFilePV(tc.PrivValidatorKeyFile(), tc.PrivValidatorStateFile()),
		key, proxy.NewLocalClientCreator(app), tm.DefaultGenesisDocProviderFunc(tc),
		tm.DefaultDBProvider, tm.DefaultMetricsProvider(tc.Instrumentation), logger)
	must(err)
	app.SetTmNode(node)
	must(node.Start())
	fmt.Printf("Go Minter Tendermint listening on %s\n", rpc)
	must(v2.Run(service.NewService(app, app.RpcClient(), node, cfg, "go-oracle", app.RewardCounter()),
		grpcAddr, httpAddr, logger))
}

type checkpoint struct {
	Height  uint64
	AppHash string
}

func restoreCheckpoint(home, snapshot, manifest string, logger log.Logger) (*minter.Blockchain, *config.Config, checkpoint) {
	var manifestData checkpoint
	raw, err := os.ReadFile(manifest)
	must(err)
	must(json.Unmarshal(raw, &manifestData))
	must(os.Mkdir(home, 0755))
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(home, ""), cfg, context.Background(), 0, 0, logger)
	root, err := app.RestoreCheckpointFile(snapshot, manifestData.Height)
	must(err)
	if hex.EncodeToString(root) != manifestData.AppHash {
		panic("Go checkpoint AppHash differs from the manifest")
	}
	return app, cfg, manifestData
}

func serveCheckpoint(home, snapshot, manifest, grpcAddr, httpAddr, samplePath, history, historyRPC string) {
	logger := log.NewNopLogger()
	app, cfg, checkpoint := restoreCheckpoint(home, snapshot, manifest, logger)
	var client *local.Local
	if history != "" {
		client = serveTransactionHistory(history, historyRPC, checkpoint.Height, logger)
	}

	state := app.CurrentState()
	candidates := state.Candidates().GetCandidates()
	for _, candidate := range candidates {
		for _, stake := range state.Candidates().GetStakes(candidate.PubKey) {
			coin := state.Coins().GetCoin(stake.Coin)
			if stake.Coin == 0 || coin.Crr() == 0 || stake.Value.Sign() <= 0 {
				continue
			}
			sample, err := json.Marshal(map[string]any{
				"owner": candidate.OwnerAddress.String(), "delegator": stake.Owner.String(),
				"coinId": coin.ID().Uint32(), "symbol": coin.GetFullSymbol(),
				"height": checkpoint.Height, "appHash": checkpoint.AppHash,
			})
			must(err)
			must(os.WriteFile(samplePath, sample, 0644))
			must(v2.Run(service.NewService(app, client, nil, cfg, "go-oracle", app.RewardCounter()),
				grpcAddr, httpAddr, logger))
			return
		}
	}
	panic("checkpoint did not contain an active reserve-coin stake")
}

// Serve the Events API from a restored Go app while replaying ABCI requests.
func serveEvents(home, snapshot, manifest, abciAddr, grpcAddr, httpAddr string) {
	logger := log.NewNopLogger()
	app, cfg, _ := restoreCheckpoint(home, snapshot, manifest, logger)
	endpoint, err := abciserver.NewServer("tcp://"+abciAddr, "socket", app)
	must(err)
	endpoint.SetLogger(logger)
	must(endpoint.Start())
	defer endpoint.Stop()
	fmt.Printf("Go ABCI 0.34 listening on %s\n", abciAddr)
	must(v2.Run(service.NewService(app, nil, nil, cfg, "go-oracle", app.RewardCounter()),
		grpcAddr, httpAddr, logger))
}

// Replay authenticated DeliverTx receipts into the original Tendermint index.
// Both the Go service and Rust's companion RPC query this same history.
func serveTransactionHistory(trace, address string, through uint64, logger log.Logger) *local.Local {
	file, err := os.Open(trace)
	must(err)
	defer file.Close()
	stream, err := gzip.NewReader(file)
	must(err)
	defer stream.Close()
	source := json.NewDecoder(stream)
	index := kv.NewTxIndex(db.NewMemDB())
	var last uint64
	for {
		var row struct {
			Height    uint64 `json:"height"`
			DeliverTx []struct {
				Request  []byte `json:"request"`
				Response []byte `json:"response"`
			} `json:"deliver_tx"`
		}
		err := source.Decode(&row)
		if err == io.EOF {
			break
		}
		must(err)
		if row.Height > through {
			break
		}
		if last != 0 && row.Height != last+1 {
			panic("nonconsecutive Go transaction history")
		}
		last = row.Height
		for position, delivery := range row.DeliverTx {
			var request abci.RequestDeliverTx
			var response abci.ResponseDeliverTx
			must(request.Unmarshal(delivery.Request))
			must(response.Unmarshal(delivery.Response))
			must(index.Index(&abci.TxResult{
				Height: int64(row.Height), Index: uint32(position), Tx: request.Tx, Result: response,
			}))
		}
	}
	if last != through {
		panic("transaction history does not reach the checkpoint")
	}
	core.SetEnvironment(&core.Environment{TxIndexer: index, Logger: logger})
	mux := http.NewServeMux()
	rpcserver.RegisterRPCFuncs(mux, map[string]*rpcserver.RPCFunc{"tx": core.Routes["tx"]}, logger)
	listener, err := net.Listen("tcp", address)
	must(err)
	go func() { must(http.Serve(listener, mux)) }()
	// Tx is a direct local call and requires only the installed index.
	return &local.Local{Logger: logger}
}

type query struct {
	Method        string
	Address       string
	Symbol        string
	ID            uint64
	Height        uint64
	TargetVersion uint64
	Delegated     bool
	Hash          string
	Query         string
	Page          int32
	PerPage       int32
	Fields        []pb.BlockField
	FailedTxs     bool
	Events        bool
	Search        []string
	Tx            string
}

type reply struct {
	Code    int               `json:"code"`
	Message string            `json:"message,omitempty"`
	Details []json.RawMessage `json:"details,omitempty"`
	Body    json.RawMessage   `json:"body,omitempty"`
}

var marshal = protojson.MarshalOptions{UseProtoNames: true, EmitUnpopulated: true}

func queryAll(address, cases string) {
	data, err := os.ReadFile(cases)
	must(err)
	var queries []query
	must(json.Unmarshal(data, &queries))
	conn, err := grpc.Dial(address, grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(64<<20)))
	must(err)
	defer conn.Close()
	client := pb.NewApiServiceClient(conn)
	replies := make([]reply, 0, len(queries))
	for _, q := range queries {
		replies = append(replies, call(client, q))
	}
	must(json.NewEncoder(os.Stdout).Encode(replies))
}

func call(client pb.ApiServiceClient, q query) reply {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	var result proto.Message
	var err error
	switch q.Method {
	case "Status":
		result, err = client.Status(ctx, &emptypb.Empty{})
	case "VersionNetwork":
		result, err = client.VersionNetwork(ctx, &pb.VersionNetworkRequest{})
	case "PriceCommission":
		result, err = client.PriceCommission(ctx, &pb.PriceCommissionRequest{Height: q.Height})
	case "CommissionVotes":
		result, err = client.CommissionVotes(ctx, &pb.CommissionVotesRequest{Height: q.Height, TargetVersion: q.TargetVersion})
	case "UpdateVotes":
		result, err = client.UpdateVotes(ctx, &pb.UpdateVotesRequest{Height: q.Height, TargetVersion: q.TargetVersion})
	case "Halts":
		result, err = client.Halts(ctx, &pb.HaltsRequest{Height: q.Height})
	case "Address":
		result, err = client.Address(ctx, &pb.AddressRequest{Address: q.Address, Height: q.Height, Delegated: q.Delegated})
	case "CoinInfo":
		result, err = client.CoinInfo(ctx, &pb.CoinInfoRequest{Symbol: q.Symbol, Height: q.Height})
	case "CoinInfoById":
		result, err = client.CoinInfoById(ctx, &pb.CoinIdRequest{Id: q.ID, Height: q.Height})
	case "Block":
		result, err = client.Block(ctx, &pb.BlockRequest{Height: q.Height, Fields: q.Fields, FailedTxs: q.FailedTxs, Events: q.Events})
	case "Events":
		result, err = client.Events(ctx, &pb.EventsRequest{Height: q.Height, Search: q.Search})
	case "Transaction":
		result, err = client.Transaction(ctx, &pb.TransactionRequest{Hash: q.Hash})
	case "Transactions":
		result, err = client.Transactions(ctx, &pb.TransactionsRequest{Query: q.Query, Page: q.Page, PerPage: q.PerPage})
	case "SendTransaction":
		result, err = client.SendTransaction(ctx, &pb.SendTransactionRequest{Tx: q.Tx})
	default:
		panic("unknown query method")
	}
	if err == nil {
		body, err := marshal.Marshal(result)
		must(err)
		return reply{Body: body}
	}
	rpc := status.Convert(err)
	out := reply{Code: int(rpc.Code()), Message: rpc.Message()}
	for _, detail := range rpc.Details() {
		message, ok := detail.(proto.Message)
		if !ok {
			panic(fmt.Sprintf("unexpected gRPC status detail %T", detail))
		}
		body, err := marshal.Marshal(message)
		must(err)
		out.Details = append(out.Details, body)
	}
	return out
}

// Read one subscription and emit its responses without waiting for it to finish.
func probeStream(transport, address, query string, count int) {
	ctx, cancel := context.WithTimeout(context.Background(), 85*time.Second)
	defer cancel()
	out := json.NewEncoder(os.Stdout)
	var mu sync.Mutex
	emit := func(value any) {
		mu.Lock()
		defer mu.Unlock()
		must(out.Encode(value))
	}
	ready := func() { emit(map[string]bool{"ready": true}) }
	var receive func() (json.RawMessage, error)
	switch transport {
	case "grpc":
		conn, err := grpc.Dial(address, grpc.WithTransportCredentials(insecure.NewCredentials()),
			grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(64<<20)))
		must(err)
		defer conn.Close()
		stream, err := pb.NewApiServiceClient(conn).Subscribe(ctx, &pb.SubscribeRequest{Query: query})
		must(err)
		ready()
		receive = func() (json.RawMessage, error) {
			response, err := stream.Recv()
			if err != nil {
				return nil, err
			}
			return marshal.Marshal(response)
		}
	case "http":
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, address, nil)
		must(err)
		trace := &httptrace.ClientTrace{WroteRequest: func(info httptrace.WroteRequestInfo) {
			if info.Err == nil {
				ready()
			}
		}}
		client := &http.Client{Transport: &http.Transport{DisableCompression: true}}
		defer client.CloseIdleConnections()
		response, err := client.Do(req.WithContext(httptrace.WithClientTrace(ctx, trace)))
		must(err)
		defer response.Body.Close()
		emit(map[string]int{"status": response.StatusCode})
		decoder := json.NewDecoder(response.Body)
		receive = func() (json.RawMessage, error) {
			var body json.RawMessage
			err := decoder.Decode(&body)
			return body, err
		}
	case "ws":
		dialer := websocket.Dialer{HandshakeTimeout: 5 * time.Second}
		conn, _, err := dialer.DialContext(ctx, address, nil)
		must(err)
		defer conn.Close()
		deadline, _ := ctx.Deadline()
		must(conn.SetReadDeadline(deadline))
		conn.SetReadLimit(64 << 20)
		ready()
		receive = func() (json.RawMessage, error) {
			_, data, err := conn.ReadMessage()
			return data, err
		}
	default:
		panic("unknown subscription transport")
	}
	for i := 0; i < count; i++ {
		body, err := receive()
		if err == io.EOF {
			emit(map[string]bool{"closed": true})
			return
		}
		if err != nil && transport == "grpc" {
			rpc := status.Convert(err)
			emit(reply{Code: int(rpc.Code()), Message: rpc.Message()})
			return
		}
		must(err)
		var envelope struct {
			Error json.RawMessage
		}
		must(json.Unmarshal(body, &envelope))
		emit(reply{Body: body})
		if envelope.Error != nil {
			return
		}
	}
}

func main() {
	args := os.Args[1:]
	switch {
	case len(args) == 8 && args[0] == "serve":
		keep, err := strconv.ParseInt(args[7], 10, 64)
		must(err)
		serve(args[1], args[2], args[3], args[4], args[5], args[6], keep)
	case len(args) == 3 && args[0] == "query":
		queryAll(args[1], args[2])
	case len(args) == 5 && args[0] == "stream":
		count, err := strconv.Atoi(args[4])
		must(err)
		probeStream(args[1], args[2], args[3], count)
	case len(args) == 7 && args[0] == "checkpoint":
		serveCheckpoint(args[1], args[2], args[3], args[4], args[5], args[6], "", "")
	case len(args) == 9 && args[0] == "checkpoint":
		serveCheckpoint(args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8])
	case len(args) == 7 && args[0] == "checkpoint-events":
		serveEvents(args[1], args[2], args[3], args[4], args[5], args[6])
	default:
		panic("usage: minter_api_oracle serve HOME FIXTURE RPC P2P GRPC HTTP KEEP | checkpoint HOME SNAPSHOT MANIFEST GRPC HTTP SAMPLE.json [TRACE HISTORY_RPC] | checkpoint-events HOME SNAPSHOT MANIFEST ABCI GRPC HTTP | query GRPC CASES.json | stream grpc|http|ws ADDRESS QUERY COUNT")
	}
}
