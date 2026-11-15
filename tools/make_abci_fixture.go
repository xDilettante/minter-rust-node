// Run from the pinned Minter Go module with go run -mod=readonly.
package main

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"math/big"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	"github.com/MinterTeam/minter-go-node/coreV2/state/accounts"
	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/crypto"
	"github.com/MinterTeam/minter-go-node/helpers"
	"github.com/MinterTeam/minter-go-node/rlp"
	"github.com/MinterTeam/minter-go-node/tree"
	ics23 "github.com/confio/ics23/go"
	"github.com/cosmos/cosmos-sdk/snapshots"
	"github.com/cosmos/iavl"
	"github.com/tendermint/tendermint/abci/server"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	tmjson "github.com/tendermint/tendermint/libs/json"
	"github.com/tendermint/tendermint/libs/log"
	"github.com/tendermint/tendermint/privval"
	tmcrypto "github.com/tendermint/tendermint/proto/tendermint/crypto"
	tm "github.com/tendermint/tendermint/types"
	db "github.com/tendermint/tm-db"
)

const firstHeight = 101
const chain = "minter-rust-send-fixture"

type exchange struct {
	Label    string `json:"label"`
	Request  string `json:"request"`
	Response string `json:"response"`
}

type mutation struct {
	Key    string `json:"key"`
	Value  string `json:"value,omitempty"`
	Remove bool   `json:"remove,omitempty"`
}

type account struct {
	Address string `json:"address"`
	Balance string `json:"balance"`
	Nonce   uint64 `json:"nonce"`
	Coin    uint32 `json:"coin,omitempty"`
}

type block struct {
	Height    uint64     `json:"height"`
	Root      string     `json:"appHash"`
	Accounts  []account  `json:"accounts"`
	Mutations []mutation `json:"mutations"`
}

type fixture struct {
	Chain         string            `json:"chainId"`
	GenesisHeight uint64            `json:"genesisHeight"`
	GenesisRoot   string            `json:"genesisRoot"`
	GenesisWrites []mutation        `json:"genesisWrites"`
	Transactions  map[string]string `json:"transactions"`
	Exchanges     []exchange        `json:"exchanges"`
	Blocks        []block           `json:"blocks"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func (f *fixture) record(label string, request *abci.Request, response *abci.Response) {
	req, err := request.Marshal()
	check(err)
	res, err := response.Marshal()
	check(err)
	f.Exchanges = append(f.Exchanges, exchange{label, hex.EncodeToString(req), hex.EncodeToString(res)})
}

func coins(n int64) *big.Int { return helpers.BipToPip(big.NewInt(n)) }

func testKey(label string) *ecdsa.PrivateKey {
	key, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter fixture " + label)))
	check(err)
	return key
}

func send(key *ecdsa.PrivateKey, nonce uint64, to mt.Address, amount *big.Int, gas uint32, network mt.ChainID, payload string) []byte {
	return single(key, nonce, gas, network, payload, transaction.SendData{Coin: 0, To: to, Value: amount})
}

func single(key *ecdsa.PrivateKey, nonce uint64, gas uint32, network mt.ChainID, payload string, operation transaction.Data) []byte {
	return signSingle(key, nonce, gas, 0, network, payload, operation)
}

func signSingle(key *ecdsa.PrivateKey, nonce uint64, gas uint32, gasCoin mt.CoinID, network mt.ChainID, payload string, operation transaction.Data) []byte {
	data, err := rlp.EncodeToBytes(operation)
	check(err)
	tx := transaction.Transaction{
		Nonce: nonce, ChainID: network, GasPrice: gas, GasCoin: gasCoin,
		Type: operation.TxType(), Data: data, SignatureType: transaction.SigTypeSingle, Payload: []byte(payload),
	}
	check(tx.Sign(key))
	raw, err := tx.Serialize()
	check(err)
	return raw
}

func multisend(nonce uint64, items []transaction.MultisendDataItem, sender *ecdsa.PrivateKey, multisig *mt.Address, keys ...*ecdsa.PrivateKey) []byte {
	data, err := rlp.EncodeToBytes(transaction.MultisendData{List: items})
	check(err)
	tx := transaction.Transaction{
		Nonce: nonce, ChainID: mt.CurrentChainID, GasPrice: 1, GasCoin: 0,
		Type: transaction.TypeMultisend, Data: data, SignatureType: transaction.SigTypeSingle,
	}
	if multisig == nil {
		check(tx.Sign(sender))
	} else {
		tx.SignatureType = transaction.SigTypeMulti
		tx.SetMultisigAddress(*multisig)
		for _, key := range keys {
			check(tx.Sign(key))
		}
	}
	raw, err := tx.Serialize()
	check(err)
	return raw
}

func main() {
	if len(os.Args) == 2 && os.Args[1] == "alternate-header" {
		alternateHeader()
		return
	}
	if (len(os.Args) == 4 || len(os.Args) == 5) && os.Args[1] == "serve" {
		var interval int
		if len(os.Args) == 5 {
			value, err := strconv.Atoi(os.Args[4])
			check(err)
			if value < 1 {
				panic("snapshot interval must be positive")
			}
			interval = value
		}
		serve(os.Args[2], os.Args[3], interval)
		return
	}
	if len(os.Args) == 3 && os.Args[1] == "swaps" {
		makeSwapFixture(os.Args[2])
		return
	}
	if len(os.Args) != 2 {
		panic("usage: go run make_abci_fixture.go NEW_OUTPUT_DIRECTORY")
	}
	out := os.Args[1]
	check(os.Mkdir(out, 0755))
	tmp, err := os.MkdirTemp("", "minter-go-send-")
	check(err)
	defer os.RemoveAll(tmp)

	senderKey, receiverKey := testKey("sender"), testKey("receiver")
	sender := crypto.PubkeyToAddress(senderKey.PublicKey)
	receiver := crypto.PubkeyToAddress(receiverKey.PublicKey)
	stakerKey := testKey("staker")
	staker := crypto.PubkeyToAddress(stakerKey.PublicKey)
	creatorKey := testKey("asset creator")
	creator := crypto.PubkeyToAddress(creatorKey.PublicKey)
	multisig := accounts.CreateMultisigAddress(sender, 77)
	validatorKey := ed25519.GenPrivKeyFromSecret([]byte("public Minter fixture validator"))
	validatorAddress := validatorKey.PubKey().Address()
	var pubKey mt.Pubkey
	copy(pubKey[:], validatorKey.PubKey().Bytes())
	genesisTime := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)

	commission := mt.Commission{}
	fields := reflect.ValueOf(&commission).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if fields.Field(i).Kind() == reflect.String {
			fields.Field(i).SetString("0")
		}
	}
	commission.Send, commission.PayloadByte, commission.FailedTx = "12", "2", "3"
	commission.MultisendBase, commission.MultisendDelta = "17", "5"
	commission.CreateCoin, commission.CreateToken = "19", "73"
	commission.CreateTicker3, commission.CreateTicker4 = "101", "97"
	commission.CreateTicker5, commission.CreateTicker6, commission.CreateTicker7_10 = "83", "79", "71"
	commission.MintToken, commission.BurnToken = "23", "29"
	genesis := mt.AppState{
		Note: "Public keys for a local interoperability fixture", Version: minter.V3,
		Emission: coins(27045).String(), TotalSlashed: "0", MaxGas: 90000,
		PrevReward: mt.RewardPrice{Time: uint64(genesisTime.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "7"},
		Commission: commission,
		Accounts: []mt.Account{
			{Address: sender, Balance: []mt.Balance{{Coin: 0, Value: coins(40).String()}}},
			{Address: staker, Balance: []mt.Balance{{Coin: 0, Value: coins(2).String()}}},
			{Address: creator, Balance: []mt.Balance{{Coin: 0, Value: coins(25000).String()}}},
			{
				Address: multisig, Balance: []mt.Balance{{Coin: 0, Value: coins(3).String()}},
				MultisigData: &mt.Multisig{Weights: []uint64{2, 3}, Threshold: 4, Addresses: []mt.Address{sender, receiver}},
			},
		},
		Validators: []mt.Validator{{PubKey: pubKey, TotalBipStake: coins(2000).String(), AccumReward: "0", AbsentTimes: mt.NewBitArray(24)}},
		Candidates: []mt.Candidate{{
			ID: 1, PubKey: pubKey, Status: 2, Commission: 10, RewardAddress: staker,
			OwnerAddress: staker, ControlAddress: staker, TotalBipStake: coins(2000).String(),
			Stakes: []mt.Stake{{Owner: staker, Coin: 0, Value: coins(2000).String(), BipValue: coins(2000).String()}},
		}},
	}
	appBytes, err := tmjson.Marshal(genesis)
	check(err)
	doc := tm.GenesisDoc{
		ChainID: chain, InitialHeight: firstHeight, GenesisTime: genesisTime,
		ConsensusParams: tm.DefaultConsensusParams(), AppState: json.RawMessage(appBytes),
	}
	check(doc.ValidateAndComplete())
	check(doc.SaveAs(filepath.Join(out, "genesis.json")))
	pv := privval.NewFilePV(validatorKey, filepath.Join(out, "priv_validator_key.json"), filepath.Join(out, "priv_validator_state.json"))
	pv.Save()

	cfg := config.GetConfig(tmp)
	cfg.DBBackend = "memdb"
	storage := utils.NewStorage(tmp, "")
	app := minter.NewMinterBlockchain(storage, cfg, context.Background(), 0, 0, log.NewNopLogger())
	defer app.Close()

	f := fixture{Chain: chain, GenesisHeight: firstHeight - 1, Transactions: map[string]string{}}
	var writes []mutation
	iavl.MutationHook = func(_ *iavl.MutableTree, key, value []byte) {
		writes = append(writes, mutation{hex.EncodeToString(key), hex.EncodeToString(value), value == nil})
	}
	defer func() { iavl.MutationHook = nil }()

	initReq := abci.RequestInitChain{
		Time: genesisTime, ChainId: chain, InitialHeight: firstHeight, AppStateBytes: appBytes,
		ConsensusParams: tm.TM2PB.ConsensusParams(doc.ConsensusParams),
	}
	initRes := app.InitChain(initReq)
	if len(initRes.Validators) != 1 || initRes.Validators[0].Power != 100000000 {
		panic("unexpected initial validator set")
	}
	f.record("init", abci.ToRequestInitChain(initReq), abci.ToResponseInitChain(initRes))
	tree := app.DeliverState().Tree().(interface{ MutableTree() *iavl.MutableTree }).MutableTree()
	f.GenesisRoot, f.GenesisWrites = hex.EncodeToString(tree.Hash()), writes
	writes = nil

	items := []transaction.MultisendDataItem{
		{Coin: 0, To: receiver, Value: coins(2)},
		{Coin: 0, To: sender, Value: coins(1)},
		{Coin: 0, To: receiver, Value: coins(3)},
	}
	one := []transaction.MultisendDataItem{{Coin: 0, To: staker, Value: coins(1)}}
	many := make([]transaction.MultisendDataItem, 101)
	for i := range many {
		many[i] = transaction.MultisendDataItem{Coin: 0, To: receiver, Value: big.NewInt(0)}
	}
	tx := map[string][]byte{
		"transfer":     send(senderKey, 1, receiver, coins(10), 2, mt.CurrentChainID, "ok"),
		"insufficient": send(senderKey, 2, receiver, coins(100), 1, mt.CurrentChainID, "fail"),
		"wrong-nonce":  send(senderKey, 1, receiver, coins(1), 1, mt.CurrentChainID, ""),
		"wrong-chain":  send(senderKey, 2, receiver, coins(1), 1, mt.ChainTestnet, ""),
		"drain":        send(receiverKey, 1, staker, new(big.Int).Sub(coins(10), big.NewInt(12)), 1, mt.CurrentChainID, ""),
		"self":         send(senderKey, 2, sender, coins(1), 1, mt.CurrentChainID, ""),
		"gas-price":    send(senderKey, 3, receiver, coins(1), 0, mt.CurrentChainID, ""),
		"batch":        multisend(3, items, senderKey, nil),
		"empty-batch":  multisend(3, nil, senderKey, nil),
		"long-batch":   multisend(3, many, senderKey, nil),
		"unknown-coin": multisend(3, []transaction.MultisendDataItem{
			{Coin: 1234, To: receiver, Value: coins(1)},
		}, senderKey, nil),
		"multisig-batch":     multisend(1, one, nil, &multisig, senderKey, receiverKey),
		"multisig-threshold": multisend(1, one, nil, &multisig, senderKey),
		"multisig-outsider":  multisend(1, one, nil, &multisig, senderKey, stakerKey),
		"multisig-duplicate": multisend(1, one, nil, &multisig, senderKey, senderKey),
		"multisig-too-many":  multisend(1, one, nil, &multisig, senderKey, receiverKey, stakerKey),
		"multisig-missing":   multisend(1, one, nil, &staker, senderKey),
	}
	asset := func(key *ecdsa.PrivateKey, nonce uint64, gas uint32, operation transaction.Data) []byte {
		return single(key, nonce, gas, mt.CurrentChainID, "", operation)
	}
	tx["event-lock"] = asset(senderKey, 1, 1, transaction.LockData{DueBlock: uint32(firstHeight + 12), Value: coins(1)})
	token := transaction.CreateTokenData{
		Name: "Differential fixture token", Symbol: mt.StrToCoinSymbol("ABCT"),
		InitialAmount: coins(5), MaxSupply: coins(100), Mintable: true, Burnable: true,
	}
	tx["create-token"] = asset(creatorKey, 1, 2, token)
	for name, symbol := range map[string]string{"numeric-symbol": "123", "empty-symbol": "", "base-symbol": "BIP", "html-symbol": "A<B"} {
		bad := token
		bad.Symbol = mt.StrToCoinSymbol(symbol)
		tx[name] = asset(creatorKey, 1, 1, bad)
	}
	badToken := token
	badToken.Name = strings.Repeat("x", 65)
	tx["long-name"] = asset(creatorKey, 1, 1, badToken)
	badToken = token
	badToken.Mintable = false
	tx["fixed-supply-mismatch"] = asset(creatorKey, 1, 1, badToken)
	badToken = token
	badToken.InitialAmount = big.NewInt(0)
	tx["zero-initial-supply"] = asset(creatorKey, 1, 1, badToken)
	tx["duplicate-ticker"] = asset(stakerKey, 1, 1, token)
	tx["new-token-send"] = asset(creatorKey, 2, 1, transaction.SendData{Coin: 1, To: receiver, Value: coins(2)})
	tx["mint-token"] = asset(creatorKey, 3, 1, transaction.MintTokenData{Coin: 1, Value: coins(2)})
	tx["burn-token"] = asset(receiverKey, 2, 1, transaction.BurnTokenDataV260{Coin: 1, Value: coins(1)})
	tx["mint-overflow"] = asset(creatorKey, 4, 1, transaction.MintTokenData{Coin: 1, Value: coins(100)})
	tx["mint-missing-token"] = asset(creatorKey, 4, 1, transaction.MintTokenData{Coin: 999, Value: coins(1)})
	tx["mint-outsider"] = asset(receiverKey, 3, 1, transaction.MintTokenData{Coin: 1, Value: coins(1)})
	tx["burn-entire-supply"] = asset(receiverKey, 3, 1, transaction.BurnTokenDataV260{Coin: 1, Value: coins(6)})
	tx["burn-insufficient"] = asset(receiverKey, 3, 1, transaction.BurnTokenDataV260{Coin: 1, Value: coins(2)})
	coin := transaction.CreateCoinData{
		Name: "Differential reserve coin", Symbol: mt.StrToCoinSymbol("COINB"),
		InitialAmount: coins(5), InitialReserve: coins(10000), ConstantReserveRatio: 50, MaxSupply: coins(100),
	}
	tx["create-coin"] = asset(creatorKey, 4, 1, coin)
	badCoin := coin
	badCoin.InitialReserve = coins(1)
	tx["low-reserve"] = asset(creatorKey, 4, 1, badCoin)
	badCoin = coin
	badCoin.ConstantReserveRatio = 9
	tx["bad-crr"] = asset(creatorKey, 4, 1, badCoin)
	zeroPrice := token
	zeroPrice.Symbol = mt.StrToCoinSymbol("ZEROF")
	tx["zero-ticker-commission"] = asset(stakerKey, 1, 0, zeroPrice)
	tx["duplicate-zero-price-ticker"] = asset(stakerKey, 2, 1, zeroPrice)
	tx["mint-after-commit"] = asset(creatorKey, 5, 1, transaction.MintTokenData{Coin: 1, Value: coins(1)})
	tx["mint-reserve-coin"] = asset(creatorKey, 5, 1, transaction.MintTokenData{Coin: 2, Value: coins(1)})
	tx["burn-reserve-coin"] = asset(creatorKey, 5, 1, transaction.BurnTokenDataV260{Coin: 2, Value: coins(1)})
	for name, raw := range tx {
		f.Transactions[name] = hex.EncodeToString(raw)
	}

	mempool := &sync.Map{}
	checkTx := func(label, name string) {
		req := abci.RequestCheckTx{Tx: tx[name]}
		f.record(label, abci.ToRequestCheckTx(req), abci.ToResponseCheckTx(checkWithExecutor(app, mempool, req)))
	}
	sequence := map[int64][]string{
		102: {"transfer", "insufficient", "wrong-nonce", "wrong-chain"},
		103: {"drain"},
		104: {"self"},
		105: {"empty-batch", "long-batch", "unknown-coin", "batch"},
		106: {"multisig-threshold", "multisig-outsider", "multisig-duplicate", "multisig-too-many", "multisig-missing", "multisig-batch"},
		107: {"long-name", "numeric-symbol", "empty-symbol", "base-symbol", "html-symbol", "fixed-supply-mismatch",
			"zero-initial-supply", "create-token", "duplicate-ticker", "new-token-send", "mint-token", "burn-token"},
		108: {"mint-missing-token", "mint-overflow", "mint-outsider", "burn-entire-supply", "burn-insufficient",
			"low-reserve", "bad-crr", "create-coin", "zero-ticker-commission"},
		109: {"duplicate-zero-price-ticker", "mint-reserve-coin", "burn-reserve-coin", "mint-after-commit"},
	}
	clock := genesisTime
	for height := int64(firstHeight); height <= 127; height++ {
		switch height {
		case 102:
			checkTx("check-transfer", "transfer")
			checkTx("check-duplicate", "transfer")
		case 103:
			checkTx("check-drain", "drain")
		case 105:
			checkTx("check-gas-price", "gas-price")
			checkTx("check-batch", "batch")
		case 106:
			checkTx("check-multisig-threshold", "multisig-threshold")
			checkTx("check-multisig-batch", "multisig-batch")
		case 107:
			checkTx("check-create-token", "create-token")
		case 108:
			checkTx("check-create-coin", "create-coin")
			checkTx("check-burn-entire-supply", "burn-entire-supply")
		}
		if height < 105 {
			clock = clock.Add(11 * time.Second)
		} else {
			clock = clock.Add(3 * time.Second)
		}
		var votes []abci.VoteInfo
		if height != firstHeight {
			votes = []abci.VoteInfo{{
				Validator: abci.Validator{Address: validatorAddress.Bytes(), Power: 100000000}, SignedLastBlock: height != 103,
			}}
		}
		header := tm.Header{ChainID: chain, Height: height, Time: clock, ProposerAddress: validatorAddress}
		begin := abci.RequestBeginBlock{
			Header:         *header.ToProto(),
			LastCommitInfo: abci.LastCommitInfo{Votes: votes},
		}
		f.record(fmt.Sprintf("begin-%d", height), abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(app.BeginBlock(begin)))
		for _, name := range sequence[height] {
			req := abci.RequestDeliverTx{Tx: tx[name]}
			f.record("deliver-"+name, abci.ToRequestDeliverTx(req), abci.ToResponseDeliverTx(app.DeliverTx(req)))
		}
		end := abci.RequestEndBlock{Height: height}
		f.record(fmt.Sprintf("end-%d", height), abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(app.EndBlock(end)))
		commit := app.Commit()
		f.record(fmt.Sprintf("commit-%d", height), abci.ToRequestCommit(), abci.ToResponseCommit(commit))
		result := block{Height: uint64(height), Root: hex.EncodeToString(commit.Data), Mutations: writes}
		writes = nil
		balances := app.CurrentState().Accounts()
		readBalance := func(address mt.Address, coin mt.CoinID) {
			result.Accounts = append(result.Accounts, account{
				Address: hex.EncodeToString(address[:]), Balance: balances.GetBalance(address, coin).String(),
				Nonce: balances.GetNonce(address), Coin: coin.Uint32(),
			})
		}
		for _, address := range []mt.Address{sender, receiver, staker, creator, {}} {
			readBalance(address, 0)
		}
		for _, id := range []mt.CoinID{1, 2, 3} {
			readBalance(creator, id)
			readBalance(receiver, id)
		}
		readBalance(multisig, 0)
		f.Blocks = append(f.Blocks, result)
		info := abci.RequestInfo{}
		f.record(fmt.Sprintf("info-%d", height), abci.ToRequestInfo(info), abci.ToResponseInfo(app.Info(info)))
		mempool = &sync.Map{}
	}
	data, err := json.MarshalIndent(f, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(out, "go-abci-v034.json"), append(data, '\n'), 0644))
	fmt.Printf("Go Minter executed %d blocks, final AppHash %s\n", len(f.Blocks), f.Blocks[len(f.Blocks)-1].Root)
}

// Sign an alternative header only for the deterministic local fixture network.
func alternateHeader() {
	input, err := io.ReadAll(io.LimitReader(os.Stdin, 1<<20))
	check(err)
	var signed tm.SignedHeader
	check(tmjson.Unmarshal(input, &signed))
	key := ed25519.GenPrivKeyFromSecret([]byte("public Minter fixture validator"))
	if signed.Header == nil || signed.Commit == nil || signed.ChainID != chain ||
		len(signed.AppHash) != 32 || len(signed.Commit.Signatures) != 1 ||
		!bytes.Equal(signed.Commit.Signatures[0].ValidatorAddress, key.PubKey().Address()) {
		panic("expected a single-validator local fixture header")
	}
	signed.AppHash[0] ^= 1
	signed.Commit.BlockID.Hash = signed.Header.Hash()
	sig, err := key.Sign(signed.Commit.VoteSignBytes(signed.ChainID, 0))
	check(err)
	signed.Commit.Signatures[0].Signature = sig
	check(tm.NewValidatorSet([]*tm.Validator{tm.NewValidator(key.PubKey(), 100000000)}).
		VerifyCommitLight(signed.ChainID, signed.Commit.BlockID, signed.Height, signed.Commit))
	encoded, err := tmjson.Marshal(signed)
	check(err)
	fmt.Println(string(encoded))
}

func makeSwapFixture(out string) {
	// Start after the historical LockStake activation without crossing a payout height.
	const firstHeight = 10197400
	check(os.Mkdir(out, 0755))
	home, err := os.MkdirTemp("", "minter-go-swap-")
	check(err)
	defer os.RemoveAll(home)
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(home, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())

	const network = "minter-mainnet-5"
	traderKey, sellerKey, smallKey := testKey("swap trader"), testKey("swap seller"), testKey("reserve fee capped")
	releasedKey := testKey("frozen funds recipient")
	trader, seller := crypto.PubkeyToAddress(traderKey.PublicKey), crypto.PubkeyToAddress(sellerKey.PublicKey)
	small := crypto.PubkeyToAddress(smallKey.PublicKey)
	released := crypto.PubkeyToAddress(releasedKey.PublicKey)
	validator := ed25519.GenPrivKeyFromSecret([]byte("public Minter swap fixture validator")).PubKey()
	var pubKey mt.Pubkey
	copy(pubKey[:], validator.Bytes())
	unbondedFrom := mt.Pubkey{1}
	moveTarget := mt.Pubkey{2}
	deletedKey := mt.Pubkey{3}
	jailedKey := mt.Pubkey{5}
	const releaseHeight = firstHeight + 21
	const firstUnbond = firstHeight + 43
	const lockRelease = firstHeight + 58
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	commission := mt.Commission{}
	fields := reflect.ValueOf(&commission).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if fields.Field(i).Kind() == reflect.String {
			fields.Field(i).SetString("0")
		}
	}
	commission.Send, commission.FailedTx = coins(3).String(), coins(1).String()
	commission.BuyPoolBase, commission.SellPoolBase, commission.SellAllPoolBase = coins(2).String(), coins(2).String(), coins(2).String()
	commission.AddLimitOrder, commission.RemoveLimitOrder = coins(1).String(), coins(1).String()
	commission.AddLiquidity, commission.RemoveLiquidity = coins(2).String(), coins(2).String()
	commission.BuyBancor, commission.SellBancor, commission.SellAllBancor = coins(3).String(), coins(3).String(), coins(3).String()
	commission.Unbond, commission.MoveStake = coins(3).String(), coins(3).String()
	commission.LockStake, commission.Lock = coins(2).String(), coins(2).String()
	commission.Delegate = coins(3).String()
	commission.SetCandidateOn, commission.SetCandidateOff = coins(2).String(), coins(2).String()
	reserveDust := big.NewInt(1_000_000_000_000_000)
	genesis := mt.AppState{
		Note: "Public fixture keys", Version: minter.V3, Emission: coins(173873).String(),
		TotalSlashed: "0", MaxGas: 100000, Commission: commission,
		PrevReward: mt.RewardPrice{Time: uint64(now.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "7"},
		Validators: []mt.Validator{{PubKey: pubKey, TotalBipStake: coins(2000).String(), AccumReward: "0", AbsentTimes: mt.NewBitArray(24)}},
		Candidates: []mt.Candidate{
			{ID: 1, PubKey: pubKey, Status: 2, Commission: 10, RewardAddress: trader, OwnerAddress: trader,
				ControlAddress: trader, TotalBipStake: coins(2000).String(),
				Stakes: []mt.Stake{{Owner: trader, Coin: 0, Value: coins(2000).String(), BipValue: coins(2000).String()}}},
			{ID: 2, PubKey: moveTarget, Status: 1, Commission: 10, RewardAddress: trader, OwnerAddress: trader,
				ControlAddress: seller, TotalBipStake: coins(1800).String(),
				Stakes: []mt.Stake{{Owner: trader, Coin: 0, Value: coins(1800).String(), BipValue: coins(1800).String()}}},
			{ID: 4, PubKey: jailedKey, Status: 1, Commission: 10, RewardAddress: trader, OwnerAddress: trader,
				ControlAddress: trader, TotalBipStake: "0", JailedUntil: firstHeight + 100},
		},
		DeletedCandidates: []mt.DeletedCandidate{{ID: 3, PubKey: deletedKey}},
		Waitlist: []mt.Waitlist{
			{CandidateID: 1, Owner: trader, Value: coins(40).String()},
			{CandidateID: 1, Owner: trader, Coin: 4, Value: coins(2).String()},
			{CandidateID: 1, Owner: seller, Value: coins(9).String()},
			{CandidateID: 3, Owner: seller, Value: coins(4).String()},
		},
		Accounts: []mt.Account{
			{Address: trader, Balance: []mt.Balance{{Coin: 0, Value: coins(20000).String()},
				{Coin: 1, Value: coins(5000).String()}, {Coin: 2, Value: coins(10000).String()},
				{Coin: 3, Value: coins(40000).String()}, {Coin: 4, Value: coins(5898).String()}}},
			{Address: seller, Balance: []mt.Balance{{Coin: 1, Value: coins(3000).String()}, {Coin: 4, Value: coins(100).String()}}},
			{Address: small, Balance: []mt.Balance{{Coin: 2, Value: reserveDust.String()}}},
		},
		FrozenFunds: []mt.FrozenFund{
			{Height: releaseHeight, Address: released, Value: coins(5).String()},
			{Height: releaseHeight, Address: released, CandidateKey: &unbondedFrom, CandidateID: 2, Value: coins(7).String()},
			{Height: releaseHeight, Address: trader, CandidateKey: &unbondedFrom, CandidateID: 2, Value: coins(6).String(), MoveToCandidateID: 1},
			{Height: firstUnbond + mt.GetUnbondPeriod(), Address: released, Value: coins(2).String()},
		},
		Coins: []mt.Coin{
			{ID: 1, Symbol: mt.StrToCoinSymbol("TST"), Name: "Fixture token", Volume: coins(28000).String(), Reserve: "0", MaxSupply: coins(100000).String(), OwnerAddress: &trader},
			{ID: 2, Symbol: mt.StrToCoinSymbol("RSV"), Name: "Fixture reserve", Volume: new(big.Int).Add(coins(10000), reserveDust).String(),
				Reserve: coins(20000).String(), Crr: 50, MaxSupply: coins(100000).String(), OwnerAddress: &trader},
			{ID: 3, Symbol: mt.StrToCoinSymbol("LP-1"), Name: "Liquidity share for BIP and TST", Volume: coins(40000).String(), Reserve: "0", MaxSupply: coins(100000).String()},
			{ID: 4, Symbol: mt.StrToCoinSymbol("RSB"), Name: "Second reserve coin", Volume: coins(6000).String(),
				Reserve: coins(30000).String(), Crr: 80, MaxSupply: coins(100000).String(), OwnerAddress: &trader},
		},
		Pools: []mt.Pool{{Coin0: 0, Coin1: 1, Reserve0: coins(100000).String(), Reserve1: coins(20000).String(), ID: 1}},
	}
	check(genesis.Verify())
	raw, err := tmjson.Marshal(genesis)
	check(err)
	app.InitChain(abci.RequestInitChain{Time: now, ChainId: network, InitialHeight: firstHeight, AppStateBytes: raw})
	check(app.ConfigureCheckpointExport(filepath.Join(out, "checkpoint")))
	f := fixture{Chain: network, GenesisHeight: firstHeight}
	type exercise struct {
		name string
		code uint32
		raw  []byte
	}
	sign := func(key *ecdsa.PrivateKey, nonce uint64, gasCoin mt.CoinID, data transaction.Data) []byte {
		return signSingle(key, nonce, 1, gasCoin, mt.CurrentChainID, "", data)
	}
	sendBip := func(amount int64) transaction.SendData {
		return transaction.SendData{Coin: 0, To: seller, Value: coins(amount)}
	}
	sellOrder := func(sell, buy int64) transaction.AddLimitOrderData {
		return transaction.AddLimitOrderData{CoinToSell: 1, ValueToSell: coins(sell), CoinToBuy: 0, ValueToBuy: coins(buy)}
	}
	unbond := func(key mt.Pubkey, value int64) transaction.UnbondDataV3 {
		return transaction.UnbondDataV3{PubKey: key, Value: coins(value)}
	}
	cases := []exercise{
		{"sell-all-overlapping-fee", 0, sign(sellerKey, 1, 99, transaction.SellAllSwapPoolDataV260{Coins: []mt.CoinID{1, 0}, MinimumValueToBuy: coins(1)})},
		{"buy-overlapping-fee", 0, sign(traderKey, 1, 1, transaction.BuySwapPoolDataV260{Coins: []mt.CoinID{0, 1}, ValueToBuy: coins(1), MaximumValueToSell: coins(200)})},
		{"reserve-fee", 0, sign(traderKey, 2, 2, sendBip(1))},
		{"capped-reserve-fee", 107, sign(smallKey, 1, 2, sendBip(1))},
		{"failed-pool-fee", 107, sign(traderKey, 3, 1, sendBip(100000))},
		{"buy-maximum", 302, sign(traderKey, 3, 0, transaction.BuySwapPoolDataV260{Coins: []mt.CoinID{0, 1}, ValueToBuy: coins(1), MaximumValueToSell: big.NewInt(1)})},
		{"sell-minimum", 303, sign(traderKey, 3, 0, transaction.SellSwapPoolDataV260{Coins: []mt.CoinID{1, 0}, ValueToSell: coins(1), MinimumValueToBuy: coins(100000)})},
		{"sell-after-rejections", 0, sign(traderKey, 3, 0, transaction.SellSwapPoolDataV260{Coins: []mt.CoinID{1, 0}, ValueToSell: coins(1), MinimumValueToBuy: big.NewInt(1)})},
		{"add-first-order", 0, sign(traderKey, 4, 0, sellOrder(100, 400))},
		{"add-same-price-order", 0, sign(traderKey, 5, 0, sellOrder(100, 400))},
		{"buy-partial-order", 0, sign(sellerKey, 2, 0, transaction.BuySwapPoolDataV260{Coins: []mt.CoinID{0, 1}, ValueToBuy: coins(700), MaximumValueToSell: coins(10000)})},
		{"sell-through-orders", 0, sign(sellerKey, 3, 0, transaction.SellSwapPoolDataV260{Coins: []mt.CoinID{0, 1}, ValueToSell: coins(1000), MinimumValueToBuy: big.NewInt(1)})},
		{"add-order-for-cancel", 0, sign(traderKey, 6, 0, sellOrder(30, 150))},
		{"cancel-another-owner", 712, sign(sellerKey, 4, 0, transaction.RemoveLimitOrderData{ID: 3})},
		{"cancel-own-order", 0, sign(traderKey, 7, 0, transaction.RemoveLimitOrderData{ID: 3})},
		{"cancel-missing-order", 711, sign(sellerKey, 4, 0, transaction.RemoveLimitOrderData{ID: 3})},
		{"reject-order-price", 713, sign(traderKey, 8, 0, sellOrder(1, 1))},
		{"reject-order-volume", 714, sign(traderKey, 8, 0, transaction.AddLimitOrderData{CoinToSell: 1, ValueToSell: big.NewInt(1), CoinToBuy: 0, ValueToBuy: big.NewInt(4)})},
		{"add-reverse-order", 0, sign(traderKey, 8, 0, transaction.AddLimitOrderData{CoinToSell: 0, ValueToSell: coins(780), CoinToBuy: 1, ValueToBuy: coins(200)})},
		{"pay-gas-through-order", 0, signSingle(sellerKey, 4, 1000, 1, mt.CurrentChainID, "", sendBip(1))},
		{"mature-frozen-funds", 0, sign(sellerKey, 5, 0, sendBip(1))},
		{"spend-released-funds", 0, sign(releasedKey, 1, 0, sendBip(9))},
		{"add-liquidity-fee-order", 0, sign(traderKey, 9, 0, transaction.AddLimitOrderData{CoinToSell: 0, ValueToSell: coins(750), CoinToBuy: 1, ValueToBuy: coins(200)})},
		{"add-liquidity-gas-through-order", 0, signSingle(traderKey, 10, 1500, 1, mt.CurrentChainID, "", transaction.AddLiquidityDataV260{Coin0: 0, Coin1: 1, Volume0: coins(1000), MaximumVolume1: coins(1000)})},
		{"add-reverse-liquidity", 0, sign(traderKey, 11, 1, transaction.AddLiquidityDataV260{Coin0: 1, Coin1: 0, Volume0: coins(20), MaximumVolume1: coins(200)})},
		{"remove-reverse-liquidity", 0, sign(traderKey, 12, 1, transaction.RemoveLiquidityV240{Coin0: 1, Coin1: 0, Liquidity: coins(100), MinimumVolume0: big.NewInt(1), MinimumVolume1: big.NewInt(1)})},
		{"add-liquidity-over-maximum", 702, sign(traderKey, 13, 0, transaction.AddLiquidityDataV260{Coin0: 0, Coin1: 1, Volume0: coins(1000), MaximumVolume1: big.NewInt(1)})},
		{"remove-liquidity-below-minimum", 705, sign(traderKey, 13, 0, transaction.RemoveLiquidityV240{Coin0: 0, Coin1: 1, Liquidity: coins(100), MinimumVolume0: coins(1000), MinimumVolume1: coins(1000)})},
		{"remove-zero-liquidity", 106, sign(traderKey, 13, 0, transaction.RemoveLiquidityV240{Coin0: 0, Coin1: 1, Liquidity: big.NewInt(0), MinimumVolume0: big.NewInt(0), MinimumVolume1: big.NewInt(0)})},
		{"add-zero-liquidity", 704, sign(traderKey, 13, 0, transaction.AddLiquidityDataV260{Coin0: 0, Coin1: 1, Volume0: big.NewInt(0), MaximumVolume1: coins(100)})},
		{"remove-missing-pool", 701, sign(traderKey, 13, 0, transaction.RemoveLiquidityV240{Coin0: 0, Coin1: 2, Liquidity: coins(100), MinimumVolume0: big.NewInt(0), MinimumVolume1: big.NewInt(0)})},
		{"sell-base-for-reserve-fee-in-output", 0, sign(traderKey, 13, 2, transaction.SellCoinData{CoinToSell: 0, ValueToSell: coins(150), CoinToBuy: 2, MinimumValueToBuy: big.NewInt(1)})},
		{"sell-reserve-for-reserve-fee-in-input", 0, sign(traderKey, 14, 2, transaction.SellCoinData{CoinToSell: 2, ValueToSell: coins(100), CoinToBuy: 4, MinimumValueToBuy: big.NewInt(1)})},
		{"buy-reserve-with-reserve-fee-from-pool", 0, sign(traderKey, 15, 1, transaction.BuyCoinData{CoinToBuy: 2, ValueToBuy: coins(25), CoinToSell: 4, MaximumValueToSell: coins(500)})},
		{"sell-all-reserve-fee-in-input", 0, sign(sellerKey, 6, 99, transaction.SellAllCoinData{CoinToSell: 4, CoinToBuy: 0, MinimumValueToBuy: big.NewInt(1)})},
		{"sell-reserve-below-minimum", 303, sign(traderKey, 16, 2, transaction.SellCoinData{CoinToSell: 2, ValueToSell: coins(1), CoinToBuy: 0, MinimumValueToBuy: coins(100)})},
		{"buy-reserve-over-maximum", 302, sign(traderKey, 16, 0, transaction.BuyCoinData{CoinToBuy: 4, ValueToBuy: coins(10), CoinToSell: 0, MaximumValueToSell: big.NewInt(0)})},
		{"sell-reserve-below-floor", 116, sign(traderKey, 16, 0, transaction.SellCoinData{CoinToSell: 2, ValueToSell: coins(5000), CoinToBuy: 0, MinimumValueToBuy: big.NewInt(1)})},
		{"buy-base-over-reserve", 103, sign(traderKey, 16, 0, transaction.BuyCoinData{CoinToBuy: 0, ValueToBuy: coins(100000), CoinToSell: 2, MaximumValueToSell: coins(100000)})},
		{"buy-reserve-over-supply", 112, sign(traderKey, 16, 0, transaction.BuyCoinData{CoinToBuy: 4, ValueToBuy: coins(200000), CoinToSell: 0, MaximumValueToSell: coins(100000)})},
		{"sell-all-empty-balance", 107, sign(sellerKey, 7, 99, transaction.SellAllCoinData{CoinToSell: 4, CoinToBuy: 0, MinimumValueToBuy: big.NewInt(1)})},
		{"sell-token-without-reserve", 200, sign(traderKey, 16, 0, transaction.SellCoinData{CoinToSell: 1, ValueToSell: coins(1), CoinToBuy: 0, MinimumValueToBuy: big.NewInt(1)})},
		{"unbond-partial-waitlist", 0, sign(traderKey, 16, 0, unbond(pubKey, 10))},
		{"unbond-insufficient-waitlist", 412, sign(sellerKey, 8, 0, unbond(pubKey, 10))},
		{"unbond-entire-waitlist", 0, sign(sellerKey, 8, 0, unbond(pubKey, 9))},
		{"unbond-missing-stake", 404, sign(sellerKey, 9, 0, unbond(pubKey, 1))},
		{"unbond-waitlist-and-stake", 0, sign(traderKey, 17, 0, unbond(pubKey, 40))},
		{"unbond-active-stake-pool-fee", 0, sign(traderKey, 18, 1, unbond(pubKey, 1))},
		{"unbond-insufficient-stake", 405, sign(traderKey, 19, 0, unbond(pubKey, 2000))},
		{"lock-stake", 0, sign(traderKey, 19, 0, transaction.LockStakeData{})},
		{"unbond-locked-stake", 416, sign(traderKey, 20, 0, unbond(pubKey, 1))},
		{"move-locked-stake", 0, sign(traderKey, 20, 0, transaction.MoveStakeData{FromPubKey: pubKey, ToPubKey: moveTarget, Value: coins(10)})},
		{"move-to-same-candidate", 417, sign(traderKey, 21, 0, transaction.MoveStakeData{FromPubKey: pubKey, ToPubKey: pubKey, Value: coins(1)})},
		{"move-to-unregistered-key", 0, sign(traderKey, 21, 0, transaction.MoveStakeData{FromPubKey: pubKey, ToPubKey: mt.Pubkey{4}, Value: coins(1)})},
		{"lock-balance", 0, sign(traderKey, 22, 0, transaction.LockData{DueBlock: lockRelease, Value: coins(40)})},
		{"lock-past-height", 123, sign(traderKey, 23, 0, transaction.LockData{DueBlock: lockRelease - 2, Value: coins(40)})},
		{"lock-insufficient-balance", 107, sign(traderKey, 23, 0, transaction.LockData{DueBlock: lockRelease, Value: coins(100000)})},
		{"spend-after-lock-release", 0, sign(traderKey, 23, 0, sendBip(1))},
		{"candidate-online-unrelated-sender", 406, sign(sellerKey, 9, 0, transaction.SetCandidateOnData{PubKey: jailedKey})},
		{"candidate-online-jailed", 414, sign(traderKey, 24, 0, transaction.SetCandidateOnData{PubKey: jailedKey})},
		{"candidate-online-by-controller", 0, sign(sellerKey, 9, 0, transaction.SetCandidateOnData{PubKey: moveTarget})},
		{"candidate-offline-by-controller", 0, sign(sellerKey, 10, 0, transaction.SetCandidateOffData{PubKey: moveTarget})},
		{"candidate-online-by-owner", 0, sign(traderKey, 24, 0, transaction.SetCandidateOnData{PubKey: moveTarget})},
		{"delegate-from-waitlist", 0, sign(traderKey, 25, 0, transaction.DelegateDataV260{PubKey: pubKey, Coin: 4, Value: big.NewInt(0)})},
		{"delegate-wallet-stake-pool-fee", 0, sign(traderKey, 26, 1, transaction.DelegateDataV260{PubKey: moveTarget, Value: coins(10)})},
		{"delegate-without-reserve", 103, sign(sellerKey, 11, 0, transaction.DelegateDataV260{PubKey: moveTarget, Coin: 1, Value: coins(1)})},
		{"delegate-zero", 408, sign(sellerKey, 11, 0, transaction.DelegateDataV260{PubKey: moveTarget, Value: big.NewInt(0)})},
		{"delegate-unknown-candidate", 403, sign(sellerKey, 11, 0, transaction.DelegateDataV260{PubKey: mt.Pubkey{9}, Value: coins(1)})},
		{"replace-offline-validator", 0, sign(traderKey, 27, 0, transaction.SetCandidateOffData{PubKey: pubKey})},
	}
	previous := app.Info(abci.RequestInfo{}).LastBlockAppHash
	for i := -1; i < len(cases); i++ {
		height := int64(firstHeight + 1 + i)
		now = now.Add(5 * time.Second)
		header := tm.Header{ChainID: network, Height: height, Time: now, ProposerAddress: validator.Address(), AppHash: previous}
		hash := sha256.Sum256([]byte(fmt.Sprintf("synthetic swap fixture block %d", height)))
		begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
		var exercises []exercise
		if i >= 0 {
			begin.LastCommitInfo.Votes = []abci.VoteInfo{{
				Validator: abci.Validator{Address: validator.Address(), Power: 100000000}, SignedLastBlock: true,
			}}
			exercises = append(exercises, cases[i])
			if height == firstUnbond {
				exercises = append(exercises, exercise{"unbond-deleted-candidate", 0, sign(sellerKey, 7, 0, unbond(deletedKey, 4))})
			}
			mempool := &sync.Map{}
			for _, entry := range exercises {
				req := abci.RequestCheckTx{Tx: entry.raw}
				res := checkWithExecutor(app, mempool, req)
				if res.Code != entry.code {
					panic(fmt.Sprintf("%s CheckTx code %d, want %d: %s", entry.name, res.Code, entry.code, res.Log))
				}
				f.record("check-"+entry.name, abci.ToRequestCheckTx(req), abci.ToResponseCheckTx(res))
			}
		}
		response := app.BeginBlock(begin)
		if i >= 0 {
			f.record(fmt.Sprintf("begin-%d", height), abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(response))
			for _, entry := range exercises {
				req := abci.RequestDeliverTx{Tx: entry.raw}
				res := app.DeliverTx(req)
				if res.Code != entry.code {
					panic(fmt.Sprintf("%s DeliverTx code %d, want %d: %s", entry.name, res.Code, entry.code, res.Log))
				}
				f.record("deliver-"+entry.name, abci.ToRequestDeliverTx(req), abci.ToResponseDeliverTx(res))
			}
		}
		end := abci.RequestEndBlock{Height: height}
		endRes := app.EndBlock(end)
		commit := app.Commit()
		previous = commit.Data
		if i < 0 {
			f.GenesisRoot = hex.EncodeToString(previous)
		} else {
			f.record(fmt.Sprintf("end-%d", height), abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(endRes))
			f.record(fmt.Sprintf("commit-%d", height), abci.ToRequestCommit(), abci.ToResponseCommit(commit))
			f.Blocks = append(f.Blocks, block{Height: uint64(height), Root: hex.EncodeToString(previous), Accounts: []account{}})
		}
	}
	check(app.Close())
	data, err := json.MarshalIndent(f, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(out, "go-swap-v034.json"), append(data, '\n'), 0644))
	fmt.Printf("Go Minter executed %d trading and frozen fund blocks, final AppHash %x\n", len(f.Blocks), previous)
}

// The reference is connected over an external socket, so it has no embedded
// Tendermint Mempool. Use the original Go executor with the idle gas price.
func checkWithExecutor(app *minter.Blockchain, mempool *sync.Map, req abci.RequestCheckTx) abci.ResponseCheckTx {
	res := minter.GetExecutor(minter.V3).RunTx(app.CurrentState(), req.Tx, nil, app.Height()+1, mempool, 1, true)
	return abci.ResponseCheckTx{
		Code: res.Code, Data: res.Data, Log: res.Log, Info: res.Info,
		GasWanted: res.GasWanted, GasUsed: res.GasUsed,
		Events: []abci.Event{{Type: "tags", Attributes: res.Tags}},
	}
}

type socketApp struct {
	*minter.Blockchain
	mempool *sync.Map
	state   db.DB
}

func (app *socketApp) CheckTx(req abci.RequestCheckTx) abci.ResponseCheckTx {
	return checkWithExecutor(app.Blockchain, app.mempool, req)
}

func (app *socketApp) Commit() abci.ResponseCommit {
	result := app.Blockchain.Commit()
	app.mempool = &sync.Map{}
	return result
}

func (app *socketApp) Query(req abci.RequestQuery) abci.ResponseQuery {
	if req.Path != "/iavl/key" {
		return app.Blockchain.Query(req)
	}
	last := app.Info(abci.RequestInfo{}).LastBlockHeight
	height := req.Height
	if height == 0 {
		height = last
	}
	if !req.Prove || height <= 0 || height > last {
		return abci.ResponseQuery{Code: 1, Log: "IAVL key query requires prove=true and a committed height"}
	}
	version := uint64(height)
	if app.InitialHeight() == 0 {
		version++
	}
	view, err := tree.NewImmutableTree(version, app.state)
	if err != nil {
		return abci.ResponseQuery{Code: 1, Log: err.Error()}
	}
	_, value := view.Get(req.Data)
	var proof *ics23.CommitmentProof
	if value == nil {
		proof, err = view.GetNonMembershipProof(req.Data)
	} else {
		proof, err = view.GetMembershipProof(req.Data)
	}
	if err != nil {
		return abci.ResponseQuery{Code: 1, Log: err.Error()}
	}
	raw, err := proof.Marshal()
	if err != nil {
		return abci.ResponseQuery{Code: 1, Log: err.Error()}
	}
	return abci.ResponseQuery{
		Height: height, Key: req.Data, Value: value,
		ProofOps: &tmcrypto.ProofOps{Ops: []tmcrypto.ProofOp{{Type: "ics23:iavl", Key: req.Data, Data: raw}}},
	}
}

func serve(home, address string, snapshotInterval int) {
	check(os.MkdirAll(home, 0755))
	cfg := config.GetConfig(home)
	cfg.DBBackend = "goleveldb"
	logger := log.NewNopLogger()
	storage := utils.NewStorage(home, "")
	_, err := storage.InitStateLevelDB("state", nil)
	check(err)
	app := minter.NewMinterBlockchain(storage, cfg, context.Background(), 0, 0, logger)
	defer app.Close()
	if snapshotInterval > 0 {
		snapshotStore, err := snapshots.NewStore(db.NewMemDB(), filepath.Join(home, "snapshots"))
		check(err)
		app.SetSnapshotStore(snapshotStore, snapshotInterval, 2)
	}
	endpoint, err := server.NewServer(address, "socket", &socketApp{
		Blockchain: app, mempool: &sync.Map{}, state: storage.StateDB(),
	})
	check(err)
	endpoint.SetLogger(logger)
	check(endpoint.Start())
	defer endpoint.Stop()
	fmt.Printf("Go ABCI 0.34 listening on %s\n", address)
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt)
	defer cancel()
	<-ctx.Done()
}
