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
	"strings"
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

var fixtureBackend = "memdb"

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

func testSigner(label string) *ecdsa.PrivateKey {
	key, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter recreation fixture " + label)))
	check(err)
	return key
}

func seed(owner mt.Address, at time.Time) mt.AppState {
	other := crypto.PubkeyToAddress(testSigner("other").PublicKey)
	poor := crypto.PubkeyToAddress(testSigner("poor").PublicKey)
	var fees mt.Commission
	fields := reflect.ValueOf(&fees).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if fields.Field(i).Kind() == reflect.String {
			fields.Field(i).SetString("0")
		}
	}
	fees.Send, fees.FailedTx, fees.EditTickerOwner = "1", "3", "17"
	fees.RecreateCoin, fees.RecreateToken = "41", "43"
	fees.CreateCoin, fees.CreateToken, fees.CreateTicker3 = "19", "73", "101"
	fees.CreateTicker4, fees.CreateTicker5, fees.CreateTicker6, fees.CreateTicker7_10 = "97", "83", "79", "71"
	fees.MintToken, fees.BurnToken, fees.CreateSwapPool = "23", "29", "37"
	validator := key(1)
	return mt.AppState{
		Note: "Public local coin recreation fixture", Version: minter.V3,
		Commission: fees, Emission: bip(10_000_000_000).String(), TotalSlashed: "0", MaxGas: 90000,
		PrevReward: mt.RewardPrice{Time: uint64(at.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "0"},
		Coins: []mt.Coin{
			{ID: 1, Name: "Current token", Symbol: mt.StrToCoinSymbol("TOK"), Volume: bip(500).String(), MaxSupply: bip(1000).String(), Mintable: true, Burnable: true, OwnerAddress: &owner},
			{ID: 2, Name: "Reserve coin", Symbol: mt.StrToCoinSymbol("RSV"), Volume: bip(100_000).String(), Reserve: bip(200_000).String(), Crr: 100, MaxSupply: bip(1_000_000).String(), OwnerAddress: &owner},
			{ID: 3, Name: "Old token", Symbol: mt.StrToCoinSymbol("TOK"), Version: 7, Volume: "9", MaxSupply: "100", Mintable: true, Burnable: true, OwnerAddress: &owner},
			{ID: 4, Name: "No owner token", Symbol: mt.StrToCoinSymbol("NUL"), Volume: "10", MaxSupply: "10"},
			{ID: 5, Name: "Poor wallet token", Symbol: mt.StrToCoinSymbol("PRT"), Volume: "10", MaxSupply: "10", OwnerAddress: &poor},
		},
		Accounts: []mt.Account{
			{Address: owner, Balance: []mt.Balance{{Coin: 0, Value: bip(1_000_000).String()}, {Coin: 1, Value: bip(500).String()}, {Coin: 2, Value: bip(90_000).String()}, {Coin: 3, Value: "9"}, {Coin: 4, Value: "10"}}},
			{Address: other, Balance: []mt.Balance{{Coin: 0, Value: bip(100_000).String()}, {Coin: 2, Value: bip(10_000).String()}}},
			{Address: poor, Balance: []mt.Balance{{Coin: 0, Value: "1"}, {Coin: 5, Value: "10"}}},
		},
		Candidates: []mt.Candidate{{ID: 1, PubKey: validator, Status: 2, Commission: 10, RewardAddress: other, OwnerAddress: other, ControlAddress: other,
			TotalBipStake: bip(2000).String(), Stakes: []mt.Stake{{Owner: other, Coin: 0, Value: bip(2000).String(), BipValue: bip(2000).String()}}}},
		Validators: []mt.Validator{{PubKey: validator, TotalBipStake: bip(2000).String(), AccumReward: "0", AbsentTimes: mt.NewBitArray(24)}},
	}
}

func token(symbol string) transaction.RecreateTokenData {
	return transaction.RecreateTokenData{Name: "Recreated token", Symbol: mt.StrToCoinSymbol(symbol), InitialAmount: bip(10), MaxSupply: bip(100), Mintable: true, Burnable: true}
}
func reserve(symbol string) transaction.RecreateCoinData {
	return transaction.RecreateCoinData{Name: "Recreated coin", Symbol: mt.StrToCoinSymbol(symbol), InitialAmount: bip(100), InitialReserve: bip(10_000), ConstantReserveRatio: 100, MaxSupply: bip(10_000)}
}
func act(data transaction.Data, code uint32) action { return action{data: data, code: code} }
func by(data transaction.Data, gas mt.CoinID, signer *ecdsa.PrivateKey, code uint32) action {
	return action{data: data, gasCoin: gas, signer: signer, code: code}
}
func owner(symbol string, address mt.Address) transaction.EditCoinOwnerData {
	return transaction.EditCoinOwnerData{Symbol: mt.StrToCoinSymbol(symbol), NewOwner: address}
}

func newApp(home string) *minter.Blockchain {
	cfg := config.GetConfig(home)
	cfg.DBBackend = fixtureBackend
	storage := utils.NewStorage(cfg.RootDir, "")
	if fixtureBackend == "goleveldb" {
		_, err := storage.InitStateLevelDB("state", nil)
		check(err)
	}
	return minter.NewMinterBlockchain(storage, cfg, context.Background(), 0, 0, log.NewNopLogger())
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
	hash := sha256.Sum256([]byte(fmt.Sprintf("local coin recreation fixture %s %d", o.name, height)))
	begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
	if o.root != nil {
		begin.LastCommitInfo.Votes = o.votes
	}
	record(abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(o.app.BeginBlock(begin)))
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

	tree.Iterate(func(path, value []byte) bool {
		if len(path) > 0 && (path[0] == 'q' || path[0] == 'a' || path[0] == 'd') {
			b.Readings = append(b.Readings, reading{append([]byte(nil), path...), append([]byte(nil), value...)})
		}
		return false
	})
	o.blocks = append(o.blocks, b)
}

func (o *oracle) probe(data transaction.Data, code uint32) {
	rawData, err := rlp.EncodeToBytes(data)
	check(err)
	sender := crypto.PubkeyToAddress(o.signer.PublicKey)
	tx := transaction.Transaction{Nonce: o.app.DeliverState().Accounts.GetNonce(sender) + 1,
		ChainID: mt.CurrentChainID, GasPrice: 1, Type: data.TxType(), Data: rawData, SignatureType: transaction.SigTypeSingle}
	check(tx.Sign(o.signer))
	raw, err := tx.Serialize()
	check(err)
	request := abci.RequestCheckTx{Tx: raw}
	// The active Go executor with an isolated mempool and minimum gas price one.
	// Blockchain.CheckTx itself additionally requires a running Tendermint node.
	result := transaction.NewExecutorV3(transaction.GetDataV3).RunTx(o.app.CurrentState(), raw, nil, o.app.Height()+1, &sync.Map{}, 1, true)
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

func main() {
	if len(os.Args) != 2 {
		panic("usage: make_coin_recreation_fixture NEW_OUTPUT_DIRECTORY")
	}
	dir := os.Args[1]
	check(os.Mkdir(dir, 0755))
	signer, other, poor := testSigner("owner"), testSigner("other"), testSigner("poor")
	address := crypto.PubkeyToAddress(signer.PublicKey)
	otherAddress := crypto.PubkeyToAddress(other.PublicKey)
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	var scenarios []scenario

	success := start(dir, "recreation-versions-and-owner", seed(address, now), signer, now, 101)
	success.probe(token("TOK"), 0)
	success.execute(102, []action{
		act(token("TOK"), 0),
		act(transaction.MintTokenData{Coin: 1, Value: big.NewInt(1)}, 206),
		act(transaction.MintTokenData{Coin: 6, Value: big.NewInt(1)}, 0),
		act(token("TOK"), 0), act(owner("TOK", otherAddress), 0), act(token("TOK"), 206),
		by(reserve("TOK"), 0, other, 0), act(token("RSV"), 0),
	})
	success.execute(103, []action{
		act(reserve("RSV"), 0), by(token("TOK"), 0, other, 0),
		by(transaction.MintTokenData{Coin: 8, Value: big.NewInt(1)}, 0, other, 801),
		by(transaction.MintTokenData{Coin: 11, Value: big.NewInt(1)}, 0, other, 0),
		by(owner("TOK", address), 0, other, 0),
	})
	success.execute(104, []action{
		act(token("TOK"), 0),
		act(transaction.CreateTokenData{Name: "New token", Symbol: mt.StrToCoinSymbol("NEW"), InitialAmount: bip(10), MaxSupply: bip(100), Mintable: true, Burnable: true}, 0),
		act(token("NEW"), 0), act(owner("NEW", otherAddress), 0), by(reserve("NEW"), 0, other, 0),
		act(transaction.SendData{Coin: 13, To: otherAddress, Value: big.NewInt(1)}, 0),
	})
	exported := success.app.DeliverState().Export()
	exported.Version = minter.V3
	exported.PrevReward = seed(address, now).PrevReward
	exported.Emission = success.app.GetEmission().String()
	cold := start(dir, "recreation-after-go-export-import", exported, signer, now, 201)
	cold.execute(202, []action{act(token("TOK"), 0), act(owner("TOK", otherAddress), 0)})
	cold.execute(203, []action{by(reserve("TOK"), 2, other, 0), by(token("NEW"), 2, other, 0)})
	scenarios = append(scenarios, success.finish(), cold.finish())

	tokens := start(dir, "recreation-token-validation", seed(address, now), signer, now, 101)
	longName := token("MISS")
	longName.Name = strings.Repeat("N", 65)
	longName.InitialAmount = big.NewInt(0)
	fixed := token("TOK")
	fixed.Mintable = false
	small := token("TOK")
	small.InitialAmount = big.NewInt(0)
	huge := token("TOK")
	huge.MaxSupply = new(big.Int).Add(new(big.Int).Exp(big.NewInt(10), big.NewInt(33), nil), big.NewInt(1))
	tokens.probe(token("MISS"), 102)
	tokens.execute(102, []action{
		act(longName, 204), act(fixed, 205), act(small, 205), act(huge, 205),
		act(token("MISS"), 102), act(token("NUL"), 206), by(token("TOK"), 0, other, 206), act(token("TOK"), 0),
	})
	scenarios = append(scenarios, tokens.finish())

	coins := start(dir, "recreation-coin-validation-order", seed(address, now), signer, now, 101)
	badName := reserve("MISS")
	badName.Name = strings.Repeat("N", 65)
	badName.InitialAmount = big.NewInt(0)
	badAmount := reserve("TOK")
	badAmount.InitialAmount = big.NewInt(0)
	badAmount.MaxSupply = huge.MaxSupply
	badMax := reserve("TOK")
	badMax.MaxSupply = huge.MaxSupply
	badReserve := reserve("TOK")
	badReserve.InitialReserve = big.NewInt(0)
	badReserve.ConstantReserveRatio = 0
	badRatio := reserve("TOK")
	badRatio.ConstantReserveRatio = 9
	coins.probe(reserve("TOK"), 0)
	coins.execute(102, []action{
		act(badName, 204), act(badAmount, 205), act(badMax, 205), act(badReserve, 205), act(badRatio, 202),
		act(reserve("MISS"), 102), act(reserve("NUL"), 206), by(reserve("TOK"), 0, other, 206), act(reserve("TOK"), 0),
	})
	scenarios = append(scenarios, coins.finish())

	gas := start(dir, "recreation-gas-coins", seed(address, now), signer, now, 101)
	fundedReserve := reserve("RSV")
	fundedReserve.InitialReserve = bip(20_000)
	gas.execute(102, []action{
		by(token("TOK"), 2, nil, 0),
		act(transaction.CreateSwapPoolData{Coin0: 0, Coin1: 6, Volume0: bip(5), Volume1: bip(5)}, 0),
		by(reserve("TOK"), 6, nil, 0),
		by(fundedReserve, 2, nil, 0),
		by(token("RSV"), 9, nil, 0),
		act(transaction.CreateSwapPoolData{Coin0: 0, Coin1: 1, Volume0: bip(10), Volume1: bip(10)}, 0),
		by(token("RSV"), 1, nil, 0), by(token("PRT"), 0, poor, 107),
	})
	gas.execute(103, []action{by(token("TOK"), 1, nil, 0), by(reserve("TOK"), 1, nil, 0)})
	scenarios = append(scenarios, gas.finish())

	tightGenesis := seed(address, now)
	tightGenesis.Accounts[0].Balance[0].Value = bip(10_000).String()
	tight := start(dir, "recreation-reserve-and-total-balance", tightGenesis, signer, now, 101)
	tight.probe(reserve("TOK"), 107)
	tight.execute(102, []action{act(reserve("TOK"), 107), act(reserve("TOK"), 107)})
	scenarios = append(scenarios, tight.finish())

	customGenesis := seed(address, now)
	customGenesis.Commission.Coin = 2
	customGenesis.Pools = []mt.Pool{{Coin0: 0, Coin1: 2, Reserve0: bip(10_000).String(), Reserve1: bip(10_000).String(), ID: 1}}
	customGenesis.Accounts[0].Balance[0].Value = bip(990_000).String()
	customGenesis.Accounts[0].Balance[2].Value = bip(80_000).String()
	custom := start(dir, "recreation-price-coin", customGenesis, signer, now, 101)
	custom.probe(token("TOK"), 0)
	custom.execute(102, []action{act(token("TOK"), 0), by(reserve("TOK"), 2, nil, 0)})
	scenarios = append(scenarios, custom.finish())

	overflowGenesis := seed(address, now)
	overflowGenesis.Coins[2].Version = 65535
	overflow := start(dir, "recreation-version-overflow", overflowGenesis, signer, now, 101)
	overflow.execute(102, []action{act(token("TOK"), 0), act(token("TOK"), 0),
		act(transaction.MintTokenData{Coin: 1, Value: big.NewInt(1)}, 0),
		act(transaction.MintTokenData{Coin: 6, Value: big.NewInt(1)}, 0), act(owner("TOK", otherAddress), 0)})
	overflow.execute(103, []action{by(reserve("TOK"), 0, other, 0)})
	scenarios = append(scenarios, overflow.finish())

	zeroGenesis := seed(address, now)
	zeroGenesis.Commission.RecreateCoin, zeroGenesis.Commission.RecreateToken = "0", "0"
	zero := start(dir, "recreation-zero-commission", zeroGenesis, signer, now, 101)
	zero.probe(token("TOK"), 0)
	zero.probe(reserve("TOK"), 0)
	zero.execute(102, []action{act(token("TOK"), 0), act(reserve("TOK"), 0)})
	scenarios = append(scenarios, zero.finish())

	fixtureBackend = "goleveldb"
	physical := start(dir, "recreation-after-physical-go-reopen", seed(address, now), signer, now, 101)
	physical.execute(102, []action{act(token("TOK"), 0), act(owner("TOK", otherAddress), 0)})
	check(physical.app.Close())
	physical.app = newApp(filepath.Join(physical.dir, "home"))
	cache := reflect.ValueOf(physical.app.DeliverState().Coins).Elem().FieldByName("list")
	if !cache.IsValid() || cache.Len() != 0 {
		panic("physical Go reopen did not start with an empty coin cache")
	}
	physical.probe(token("TOK"), 206)
	physical.execute(103, []action{by(reserve("TOK"), 2, other, 0), by(owner("TOK", address), 0, other, 0)})
	physical.execute(104, []action{act(token("TOK"), 0)})
	scenarios = append(scenarios, physical.finish())
	fixtureBackend = "memdb"

	mt.CurrentChainID = mt.ChainTestnet
	alternate := start(dir, "recreation-transaction-network-2", seed(address, now), signer, now, 101)
	alternate.execute(102, []action{act(token("TOK"), 0), act(reserve("TOK"), 0), by(token("TOK"), 0, other, 206)})
	scenarios = append(scenarios, alternate.finish())
	data, err := json.MarshalIndent(scenarios, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-coin-recreation-v034.json"), append(data, '\n'), 0644))
}
