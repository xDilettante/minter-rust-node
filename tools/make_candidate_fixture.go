// Run with go run -mod=readonly from the pinned Minter Go module.
package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"reflect"
	"time"

	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/crypto"
	"github.com/MinterTeam/minter-go-node/helpers"
	"github.com/MinterTeam/minter-go-node/rlp"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	tmjson "github.com/tendermint/tendermint/libs/json"
	"github.com/tendermint/tendermint/libs/log"
	tm "github.com/tendermint/tendermint/types"
)

const network = "minter-mainnet-5"
const payout = 720
const release = payout + 518400

type exchange struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type reading struct {
	Key   []byte `json:"key"`
	Value []byte `json:"value"`
}

type block struct {
	Height    int64           `json:"height"`
	Exchanges []exchange      `json:"exchanges"`
	Events    json.RawMessage `json:"events"`
	Readings  []reading       `json:"readings"`
}

type scenario struct {
	Name     string          `json:"name"`
	Manifest json.RawMessage `json:"manifest"`
	Snapshot []byte          `json:"snapshot"`
	Blocks   []block         `json:"blocks"`
}

type action struct {
	data transaction.Data
	code uint32
}

type oracle struct {
	app        *minter.Blockchain
	signer     *ecdsa.PrivateKey
	dir        string
	name       string
	checkpoint string
	at         time.Time
	root       []byte
	votes      []abci.VoteInfo
	blocks     []block
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func key(id uint32) mt.Pubkey {
	return sha256.Sum256(binary.LittleEndian.AppendUint32(nil, id))
}

func bip(value int64) *big.Int {
	return helpers.BipToPip(big.NewInt(value))
}

func seed(owner mt.Address, at time.Time) mt.AppState {
	var fees mt.Commission
	fields := reflect.ValueOf(&fees).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if field := fields.Field(i); field.Kind() == reflect.String {
			field.SetString("0")
		}
	}
	fees.DeclareCandidacy, fees.Delegate, fees.Unbond = "11", "2", "3"
	g := mt.AppState{
		Note: "Local candidate lifecycle fixture", Version: minter.V3,
		Commission: fees, Emission: bip(10_000_000).String(), TotalSlashed: "0", MaxGas: 90000,
		PrevReward: mt.RewardPrice{Time: uint64(at.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "0"},
		Coins: []mt.Coin{
			{ID: 1, Name: "Reserve fixture", Symbol: mt.StrToCoinSymbol("RSV"), Volume: bip(10_000).String(),
				Reserve: bip(10_000).String(), Crr: 100, MaxSupply: bip(100_000).String()},
			{ID: 2, Name: "Token fixture", Symbol: mt.StrToCoinSymbol("TOK"), Volume: bip(1).String(), MaxSupply: bip(1000).String()},
		},
		Accounts: []mt.Account{{Address: owner, Balance: []mt.Balance{
			{Coin: 0, Value: bip(30_000).String()}, {Coin: 1, Value: bip(9900).String()}, {Coin: 2, Value: bip(1).String()},
		}}},
		BlockListCandidates: []mt.Pubkey{key(250)},
		DeletedCandidates:   []mt.DeletedCandidate{{ID: 250, PubKey: key(250)}},
		Waitlist:            []mt.Waitlist{{CandidateID: 256, Owner: owner, Value: bip(20).String()}},
	}
	add := func(id uint32, amount int64, status uint64) {
		var address mt.Address
		binary.LittleEndian.PutUint32(address[:], id)
		stakes := []mt.Stake{{Owner: address, Value: bip(amount).String(), BipValue: bip(amount).String()}}
		if id == 255 {
			stakes = append(stakes, mt.Stake{Owner: address, Coin: 1, Value: bip(100).String(), BipValue: bip(100).String()})
			amount += 100
		}
		c := mt.Candidate{ID: uint64(id), PubKey: key(id), OwnerAddress: address, RewardAddress: address,
			ControlAddress: address, Status: status, Commission: 17, TotalBipStake: bip(amount).String(), Stakes: stakes}
		g.Candidates = append(g.Candidates, c)
		if status == 2 {
			g.Validators = append(g.Validators, mt.Validator{
				PubKey: c.PubKey, TotalBipStake: c.TotalBipStake, AccumReward: "0", AbsentTimes: mt.NewBitArray(24),
			})
		}
	}
	for id := uint32(1); id <= 97; id++ {
		add(id, 2000, 1)
	}
	add(253, 1100, 2)
	add(255, 1700, 1)
	add(256, 1700, 1)
	return g
}

func newApp(home string) *minter.Blockchain {
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	return minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
}

func start(dir, name string, g mt.AppState, signer *ecdsa.PrivateKey, at time.Time, first int64) *oracle {
	dir = filepath.Join(dir, name)
	check(os.Mkdir(dir, 0755))
	app := newApp(filepath.Join(dir, "home"))
	check(g.Verify())
	raw, err := tmjson.Marshal(g)
	check(err)
	initial := app.InitChain(abci.RequestInitChain{Time: at, ChainId: network, InitialHeight: first, AppStateBytes: raw})
	o := &oracle{app: app, signer: signer, dir: dir, name: name, checkpoint: filepath.Join(dir, "checkpoint"), at: at}
	for _, val := range initial.Validators {
		o.votes = append(o.votes, abci.VoteInfo{Validator: abci.Validator{
			Address: ed25519.PubKey(val.PubKey.GetEd25519()).Address(), Power: val.Power,
		}, SignedLastBlock: true})
	}
	check(app.ConfigureCheckpointExport(o.checkpoint))
	o.execute(first, nil)
	o.blocks = nil
	return o
}

func (o *oracle) execute(height int64, actions []action) {
	o.at = o.at.Add(5 * time.Second)
	b := block{Height: height}
	record := func(request *abci.Request, response *abci.Response) {
		req, err := request.Marshal()
		check(err)
		res, err := response.Marshal()
		check(err)
		b.Exchanges = append(b.Exchanges, exchange{req, res})
	}
	header := tm.Header{ChainID: network, Height: height, Time: o.at, AppHash: o.root}
	hash := sha256.Sum256([]byte(fmt.Sprintf("local candidate fixture %s %d", o.name, height)))
	begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
	if o.root != nil {
		begin.LastCommitInfo.Votes = o.votes
	}
	record(abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(o.app.BeginBlock(begin)))
	sender := crypto.PubkeyToAddress(o.signer.PublicKey)
	for _, a := range actions {
		data, err := rlp.EncodeToBytes(a.data)
		check(err)
		tx := transaction.Transaction{Nonce: o.app.DeliverState().Accounts.GetNonce(sender) + 1, ChainID: mt.CurrentChainID,
			GasPrice: 1, Type: a.data.TxType(), Data: data, SignatureType: transaction.SigTypeSingle}
		check(tx.Sign(o.signer))
		raw, err := tx.Serialize()
		check(err)
		req := abci.RequestDeliverTx{Tx: raw}
		res := o.app.DeliverTx(req)
		if res.Code != a.code {
			panic(fmt.Sprintf("height %d action %d: code %d, expected %d (%s)", height, len(b.Exchanges), res.Code, a.code, res.Log))
		}
		record(abci.ToRequestDeliverTx(req), abci.ToResponseDeliverTx(res))
	}
	end := abci.RequestEndBlock{Height: height}
	record(abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(o.app.EndBlock(end)))
	commit := o.app.Commit()
	o.root = commit.Data
	record(abci.ToRequestCommit(), abci.ToResponseCommit(commit))
	var err error
	b.Events, err = tmjson.Marshal(o.app.GetEventsDB().LoadEvents(uint32(height)))
	check(err)
	tree := o.app.DeliverState().Tree().GetLastImmutable()
	for _, path := range [][]byte{
		{0xd3}, {0xc5}, {0xc7}, {0xcc},
		{'c', 255, 0, 0, 0, 't'}, {'c', 0, 1, 0, 0, 's'},
		binary.BigEndian.AppendUint64([]byte{'f'}, release),
		append([]byte{'w'}, sender[:]...),
	} {
		_, value := tree.Get(path)
		b.Readings = append(b.Readings, reading{path, value})
	}
	o.blocks = append(o.blocks, b)
}

func (o *oracle) finish() scenario {
	check(o.app.Close())
	manifest, err := os.ReadFile(filepath.Join(o.checkpoint, "manifest.json"))
	check(err)
	snapshot, err := os.ReadFile(filepath.Join(o.checkpoint, "snapshot.zlib"))
	check(err)
	return scenario{o.name, manifest, snapshot, o.blocks}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: go run make_candidate_fixture.go NEW_OUTPUT_DIRECTORY")
	}
	dir := os.Args[1]
	check(os.Mkdir(dir, 0755))
	signer, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter candidate fixture wallet")))
	check(err)
	owner := crypto.PubkeyToAddress(signer.PublicKey)
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	genesis := seed(owner, now)
	app := start(dir, "declaration-and-pruning", genesis, signer, now, payout-3)
	declare := func(id uint32, commission uint32, coin mt.CoinID, amount int64, code uint32) action {
		return action{transaction.DeclareCandidacyData{Address: mt.Address{0xff}, PubKey: key(id),
			Commission: commission, Coin: coin, Stake: bip(amount)}, code}
	}
	actions := []action{
		declare(300, 17, 0, 2000, 0), declare(301, 17, 0, 2000, 0), declare(302, 17, 1, 2000, 0), declare(303, 17, 0, 2000, 0),
		declare(300, 17, 0, 1, 401), declare(250, 17, 0, 1, 410), declare(400, 101, 0, 1, 402),
		declare(400, 17, 2, 1, 200), declare(400, 17, 9, 1, 102), declare(400, 17, 0, 100_000, 107),
		{transaction.DelegateDataV260{PubKey: key(300), Coin: 0, Value: bip(1)}, 0},
	}
	app.execute(payout-2, actions)
	actions = nil
	for id := uint32(500); id < 588; id++ {
		actions = append(actions, declare(id, 17, 0, 1, 0))
	}
	actions = append(actions, declare(588, 17, 0, 0, 409), declare(588, 17, 0, 1, 0))
	app.execute(payout-1, actions)
	pruned := filepath.Join(app.dir, "pruned-checkpoint")
	check(app.app.ConfigureCheckpointExport(pruned))
	app.execute(payout, nil)
	at := app.at
	afterPruning := []action{
		declare(303, 17, 0, 2000, 410),
		{transaction.UnbondDataV3{PubKey: key(256), Coin: 0, Value: bip(10)}, 0},
		declare(700, 25, 0, 11, 0),
	}
	app.execute(payout+1, afterPruning)

	exported := app.app.DeliverState().Export()
	exported.Version = minter.V3
	exported.PrevReward = genesis.PrevReward
	exported.Emission = app.app.GetEmission().String()
	original := app.finish()
	reopened := &oracle{app: newApp(filepath.Join(dir, "reopened")), signer: signer,
		name: app.name, checkpoint: pruned, at: at, votes: app.votes}
	reopened.root, err = reopened.app.RestoreCheckpointFile(filepath.Join(pruned, "snapshot.zlib"), payout)
	check(err)
	reopened.execute(payout+1, afterPruning)
	fromCheckpoint := reopened.finish()
	fromCheckpoint.Name = "go-checkpoint-after-pruning"
	maturity := start(dir, "maturity", exported, signer, now, release-1)
	maturity.execute(release, nil)
	maturity.execute(release+1, nil)
	data, err := json.MarshalIndent([]scenario{original, fromCheckpoint, maturity.finish()}, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-candidate-lifecycle-v034.json"), append(data, '\n'), 0644))
}
