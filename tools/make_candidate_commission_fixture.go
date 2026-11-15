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
	"sync"
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
	Checks    []exchange      `json:"checks,omitempty"`
	Exchanges []exchange      `json:"exchanges"`
	Events    json.RawMessage `json:"events"`
	Readings  []reading       `json:"readings"`
}

type scenario struct {
	Name               string          `json:"name"`
	Manifest           json.RawMessage `json:"manifest"`
	Snapshot           []byte          `json:"snapshot"`
	Blocks             []block         `json:"blocks"`
	TransactionChainID uint8           `json:"transactionChainId"`
	Initialize         []exchange      `json:"initialize,omitempty"`
}

type action struct {
	data    transaction.Data
	code    uint32
	gasCoin mt.CoinID
	signer  *ecdsa.PrivateKey
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
	initialize []exchange
	checks     []exchange
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
	initRequest := abci.RequestInitChain{Time: at, ChainId: network, InitialHeight: first, AppStateBytes: raw}
	initial := app.InitChain(initRequest)
	o := &oracle{app: app, signer: signer, dir: dir, name: name, checkpoint: filepath.Join(dir, "checkpoint"), at: at}
	if mt.CurrentChainID == mt.ChainTestnet {
		request, err := abci.ToRequestInitChain(initRequest).Marshal()
		check(err)
		response, err := abci.ToResponseInitChain(initial).Marshal()
		check(err)
		o.initialize = append(o.initialize, exchange{request, response})
	}
	for _, val := range initial.Validators {
		o.votes = append(o.votes, abci.VoteInfo{Validator: abci.Validator{
			Address: ed25519.PubKey(val.PubKey.GetEd25519()).Address(), Power: val.Power,
		}, SignedLastBlock: true})
	}
	check(app.ConfigureCheckpointExport(o.checkpoint))
	o.execute(first, nil)
	if mt.CurrentChainID == mt.ChainTestnet {
		o.initialize = append(o.initialize, o.blocks[0].Exchanges...)
	}
	o.blocks = nil
	return o
}

func (o *oracle) execute(height int64, actions []action) {
	o.at = o.at.Add(5 * time.Second)
	b := block{Height: height, Checks: o.checks}
	o.checks = nil
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
		signer := o.signer
		if a.signer != nil {
			signer = a.signer
		}
		actor := crypto.PubkeyToAddress(signer.PublicKey)
		data, err := rlp.EncodeToBytes(a.data)
		check(err)
		tx := transaction.Transaction{Nonce: o.app.DeliverState().Accounts.GetNonce(actor) + 1, ChainID: mt.CurrentChainID,
			GasPrice: 1, GasCoin: a.gasCoin, Type: a.data.TxType(), Data: data, SignatureType: transaction.SigTypeSingle}
		check(tx.Sign(signer))
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
		{'c'},
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

func (o *oracle) checkCommission(data transaction.Data, code uint32) {
	rawData, err := rlp.EncodeToBytes(data)
	check(err)
	sender := crypto.PubkeyToAddress(o.signer.PublicKey)
	tx := transaction.Transaction{Nonce: o.app.DeliverState().Accounts.GetNonce(sender) + 1,
		ChainID: mt.CurrentChainID, GasPrice: 1, Type: data.TxType(), Data: rawData, SignatureType: transaction.SigTypeSingle}
	check(tx.Sign(o.signer))
	raw, err := tx.Serialize()
	check(err)
	request := abci.RequestCheckTx{Tx: raw}
	// Blockchain.CheckTx with an empty mempool and the minimum gas price of one.
	result := transaction.NewExecutor(transaction.GetDataV3).RunTx(o.app.CurrentState(), raw, nil, o.app.Height()+1, &sync.Map{}, 1, true)
	response := abci.ResponseCheckTx{Code: result.Code, Data: result.Data, Log: result.Log, Info: result.Info,
		GasWanted: result.GasWanted, GasUsed: result.GasUsed, Events: []abci.Event{{Type: "tags", Attributes: result.Tags}}}
	if response.Code != code {
		panic(fmt.Sprintf("CheckTx code %d expected %d (%s)", response.Code, code, response.Log))
	}
	req, err := abci.ToRequestCheckTx(request).Marshal()
	check(err)
	res, err := abci.ToResponseCheckTx(response).Marshal()
	check(err)
	o.checks = append(o.checks, exchange{req, res})
}

func (o *oracle) finish() scenario {
	check(o.app.Close())
	manifest, err := os.ReadFile(filepath.Join(o.checkpoint, "manifest.json"))
	check(err)
	snapshot, err := os.ReadFile(filepath.Join(o.checkpoint, "snapshot.zlib"))
	check(err)
	return scenario{o.name, manifest, snapshot, o.blocks, uint8(mt.CurrentChainID), o.initialize}
}

func commissionGenesis(owner mt.Address, at time.Time) mt.AppState {
	genesis := seed(owner, at)
	genesis.Commission.EditCandidateCommission = "5"
	for i := range genesis.Candidates {
		candidate := &genesis.Candidates[i]
		if candidate.ID == 255 || candidate.ID == 256 || candidate.ID == 97 {
			candidate.OwnerAddress, candidate.ControlAddress = owner, owner
		}
		if candidate.ID == 1 {
			candidate.ControlAddress = owner
		}
		if candidate.ID == 256 {
			candidate.Commission = 0
		}
		if candidate.ID == 97 {
			candidate.Commission = 99
		}
	}
	return genesis
}

func change(id uint32, commission uint32, code uint32) action {
	return action{data: transaction.EditCandidateCommission{PubKey: key(id), Commission: commission}, code: code}
}

func gasAction(data transaction.Data, gas mt.CoinID, signer *ecdsa.PrivateKey, code uint32) action {
	return action{data: data, gasCoin: gas, signer: signer, code: code}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: edit-candidate-commission-fixture NEW_OUTPUT_DIRECTORY")
	}
	dir := os.Args[1]
	check(os.Mkdir(dir, 0755))
	signer, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter candidate fixture wallet")))
	check(err)
	owner := crypto.PubkeyToAddress(signer.PublicKey)
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	genesis := commissionGenesis(owner, now)
	const boundary int64 = 3 * 518400
	app := start(dir, "commission-boundary", genesis, signer, now, boundary-2)
	app.execute(boundary-1, []action{
		change(999, 17, 403), change(1, 17, 406), change(255, 28, 402), change(255, 27, 413),
	})
	app.execute(boundary, []action{
		change(255, 27, 0), change(255, 17, 413), change(255, 38, 402),
		change(256, 11, 402), change(256, 10, 0), change(97, 101, 402), change(97, 100, 0),
	})
	app.execute(boundary+1, []action{change(255, 27, 413), change(256, 0, 413)})
	exported := app.app.DeliverState().Export()
	exported.Version = minter.V3
	exported.PrevReward = genesis.PrevReward
	exported.Emission = app.app.GetEmission().String()
	original := app.finish()
	second := start(dir, "commission-after-import", exported, signer, now, 2*boundary-2)
	second.execute(2*boundary-1, []action{change(255, 17, 413), change(256, 0, 413)})
	second.execute(2*boundary, []action{change(255, 17, 0), change(256, 0, 0), change(97, 90, 0)})
	second.execute(2*boundary+1, []action{change(255, 17, 413)})
	scenarios := []scenario{original, second.finish()}
	high := commissionGenesis(owner, now)
	for i := range high.Candidates {
		candidate := &high.Candidates[i]
		if candidate.ID == 255 {
			candidate.LastEditCommissionHeight = 1 << 63
		}
		if candidate.ID == 256 {
			candidate.LastEditCommissionHeight = ^uint64(0) - uint64(boundary) + uint64(boundary-2)
		}
		if candidate.ID == 97 {
			candidate.LastEditCommissionHeight = ^uint64(0)
		}
	}
	overflow := start(dir, "commission-imported-uint64", high, signer, now, boundary-2)
	overflow.execute(boundary-1, []action{change(255, 27, 413), change(256, 10, 0), change(256, 10, 413), change(97, 100, 0), change(97, 99, 413)})
	overflow.execute(boundary, []action{change(255, 17, 413), change(256, 0, 413)})
	scenarios = append(scenarios, overflow.finish())

	poorSigner, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter candidate commission poor wallet")))
	check(err)
	poor := crypto.PubkeyToAddress(poorSigner.PublicKey)
	feeGenesis := commissionGenesis(owner, now)
	// Leave room above Go's minimum reserve for commission conversion.
	feeGenesis.Coins[0].Reserve = bip(20_000).String()
	feeGenesis.Accounts = append(feeGenesis.Accounts, mt.Account{Address: poor, Balance: []mt.Balance{{Coin: 0, Value: bip(1).String()}}})
	for i := range feeGenesis.Candidates {
		candidate := &feeGenesis.Candidates[i]
		if candidate.ID == 97 || candidate.ID == 96 {
			candidate.OwnerAddress = poor
			candidate.ControlAddress = owner
		}
	}
	feeApp := start(dir, "commission-gas-coins", feeGenesis, signer, now, boundary-2)
	feeApp.execute(boundary-1, []action{gasAction(transaction.CreateSwapPoolData{Coin0: 0, Coin1: 2, Volume0: bip(10), Volume1: new(big.Int).Div(bip(1), big.NewInt(2))}, 0, nil, 0)})
	feeApp.execute(boundary, []action{
		gasAction(transaction.EditCandidateCommission{PubKey: key(255), Commission: 27}, 1, nil, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(256), Commission: 10}, 2, nil, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(97), Commission: 100}, 2, poorSigner, 107),
		gasAction(transaction.SendData{Coin: 2, To: poor, Value: new(big.Int).Div(bip(1), big.NewInt(10))}, 0, nil, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(97), Commission: 100}, 2, poorSigner, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(97), Commission: 99}, 2, poorSigner, 413),
		gasAction(transaction.SendData{Coin: 0, To: owner, Value: bip(1)}, 0, poorSigner, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(96), Commission: 17}, 0, poorSigner, 107),
		gasAction(transaction.SendData{Coin: 0, To: poor, Value: bip(1)}, 0, nil, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(96), Commission: 17}, 0, poorSigner, 0),
		gasAction(transaction.EditCandidateCommission{PubKey: key(96), Commission: 17}, 0, poorSigner, 413),
	})
	scenarios = append(scenarios, feeApp.finish())

	checked := start(dir, "commission-checktx", commissionGenesis(owner, now), signer, now, boundary-1)
	checked.checkCommission(transaction.EditCandidateCommission{PubKey: key(255), Commission: 27}, 0)
	checked.execute(boundary, []action{change(255, 27, 0)})
	checked.checkCommission(transaction.EditCandidateCommission{PubKey: key(255), Commission: 17}, 413)
	checked.execute(boundary+1, []action{change(255, 17, 413)})
	scenarios = append(scenarios, checked.finish())

	mt.CurrentChainID = mt.ChainTestnet
	// Vary the transaction-network rules in the same synthetic checkpoint namespace.
	const testBoundary int64 = 3 * (3 * (518400 / 2920))
	testApp := start(dir, "commission-transaction-network-2", commissionGenesis(owner, now), signer, now, testBoundary-2)
	testApp.execute(testBoundary-1, []action{change(255, 27, 413)})
	testApp.execute(testBoundary, []action{change(255, 27, 0), change(256, 10, 0)})
	testApp.execute(testBoundary+1, []action{change(255, 17, 413)})
	scenarios = append(scenarios, testApp.finish())
	data, err := json.MarshalIndent(scenarios, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-candidate-commission-v034.json"), append(data, '\n'), 0644))
}
