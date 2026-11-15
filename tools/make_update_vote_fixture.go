// Run with go run -mod=readonly from the pinned Minter Go module.
package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
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
	"github.com/MinterTeam/minter-go-node/coreV2/check"
	"github.com/MinterTeam/minter-go-node/coreV2/code"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	"github.com/MinterTeam/minter-go-node/coreV2/state/accounts"
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

const firstHeight = 716
const chainID = "minter-mainnet-5"

type exchange struct {
	Label    string `json:"label"`
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type block struct {
	Height    int64           `json:"height"`
	Exchanges []exchange      `json:"exchanges"`
	AppHash   string          `json:"appHash"`
	Versions  []mt.Version    `json:"versions"`
	Events    json.RawMessage `json:"events"`
}

type checkpoint struct {
	Manifest json.RawMessage `json:"manifest"`
	Snapshot []byte          `json:"snapshot"`
}

type fixture struct {
	Init        exchange   `json:"init"`
	GenesisHash string     `json:"genesisHash"`
	Blocks      []block    `json:"blocks"`
	Pending     checkpoint `json:"pending"`
	Unknown     checkpoint `json:"unknown"`
	Blocked     []byte     `json:"blocked"`
}

type attempt struct {
	label   string
	signer  int
	voter   int
	nonce   uint64
	height  uint64
	version string
	code    uint32
	check   bool
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func record(label string, request *abci.Request, response *abci.Response) exchange {
	req, err := request.Marshal()
	must(err)
	res, err := response.Marshal()
	must(err)
	return exchange{label, req, res}
}

func checkTx(app *minter.Blockchain, tx []byte, height uint64, mempool *sync.Map, label string, code uint32) exchange {
	res := minter.GetExecutor(minter.V3).RunTx(app.CurrentState(), tx, nil, height, mempool, 1, true)
	if res.Code != code {
		panic(fmt.Sprintf("CheckTx %s: expected %d, got %d %s", label, code, res.Code, res.Log))
	}
	req := abci.RequestCheckTx{Tx: tx}
	response := abci.ResponseCheckTx{
		Code: res.Code, Log: res.Log, Info: res.Info, GasWanted: res.GasWanted, GasUsed: res.GasUsed,
		Events: []abci.Event{{Type: "tags", Attributes: res.Tags}},
	}
	return record("CheckTx "+label, abci.ToRequestCheckTx(req), abci.ToResponseCheckTx(response))
}

func deliverTx(app *minter.Blockchain, tx []byte, label string, code uint32) exchange {
	req := abci.RequestDeliverTx{Tx: tx}
	res := app.DeliverTx(req)
	if res.Code != code {
		panic(fmt.Sprintf("DeliverTx %s: expected %d, got %d %s", label, code, res.Code, res.Log))
	}
	return record("DeliverTx "+label, abci.ToRequestDeliverTx(req), abci.ToResponseDeliverTx(res))
}

func key(index int) mt.Pubkey {
	return mt.Pubkey{byte(index + 1)}
}

func signedVote(signer *ecdsa.PrivateKey, vote attempt) []byte {
	return signedTransaction(signer, vote.nonce, transaction.VoteUpdateDataV230{
		Version: vote.version, PubKey: key(vote.voter), Height: vote.height,
	})
}

func signedTransaction(signer *ecdsa.PrivateKey, nonce uint64, operation transaction.Data) []byte {
	return signedTransactionInCoin(signer, nonce, 0, operation)
}

func signedTransactionInCoin(signer *ecdsa.PrivateKey, nonce uint64, gasCoin mt.CoinID, operation transaction.Data) []byte {
	data, err := rlp.EncodeToBytes(operation)
	must(err)
	tx := transaction.Transaction{
		Nonce: nonce, ChainID: mt.CurrentChainID, GasPrice: 1, GasCoin: gasCoin,
		Type: operation.TxType(), Data: data, SignatureType: transaction.SigTypeSingle,
	}
	must(tx.Sign(signer))
	raw, err := tx.Serialize()
	must(err)
	return raw
}

func genesis(signers []*ecdsa.PrivateKey, at time.Time) mt.AppState {
	var fees mt.Commission
	fields := reflect.ValueOf(&fees).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if field := fields.Field(i); field.Kind() == reflect.String {
			field.SetString("0")
		}
	}
	fees.VoteUpdate, fees.FailedTx = "4", "2"
	state := mt.AppState{
		Note: "Local Minter update vote fixture", Version: minter.V3,
		Commission: fees, MaxGas: 90000, Emission: helpers.BipToPip(big.NewInt(1_000_000)).String(),
		TotalSlashed: "0", PrevReward: mt.RewardPrice{
			Time: uint64(at.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "13579",
		},
		UpdateVotes: []mt.UpdateVote{{Height: 718, Version: minter.V310, Votes: []mt.Pubkey{key(0)}}},
	}
	stake := helpers.BipToPip(big.NewInt(2000)).String()
	for i, signer := range signers {
		owner := crypto.PubkeyToAddress(signer.PublicKey)
		state.Accounts = append(state.Accounts, mt.Account{
			Address: owner, Balance: []mt.Balance{{Coin: 0, Value: helpers.BipToPip(big.NewInt(10000)).String()}},
		})
		if i == 3 {
			continue
		}
		state.Candidates = append(state.Candidates, mt.Candidate{
			ID: uint64(i + 1), PubKey: key(i), Status: 2, Commission: 10,
			OwnerAddress: owner, RewardAddress: owner, ControlAddress: owner,
			TotalBipStake: stake, Stakes: []mt.Stake{{Owner: owner, Value: stake, BipValue: stake}},
		})
		state.Validators = append(state.Validators, mt.Validator{
			PubKey: key(i), TotalBipStake: stake, AccumReward: "0", AbsentTimes: mt.NewBitArray(24),
		})
	}
	must(state.Verify())
	return state
}

func snapshot(dir string) checkpoint {
	manifest, err := os.ReadFile(filepath.Join(dir, "manifest.json"))
	must(err)
	compressed, err := os.ReadFile(filepath.Join(dir, "snapshot.zlib"))
	must(err)
	return checkpoint{manifest, compressed}
}

func main() {
	if len(os.Args) < 2 || len(os.Args) > 3 {
		panic("usage: go run make_update_vote_fixture.go NEW_OUTPUT_DIRECTORY [--prove-stop|--halt|--prove-halt|--commission|--historical-tx]")
	}
	var mode string
	if len(os.Args) == 3 {
		mode = os.Args[2]
		if mode != "--prove-stop" && mode != "--halt" && mode != "--prove-halt" && mode != "--commission" && mode != "--historical-tx" {
			panic("unknown fixture mode " + mode)
		}
	}
	out := os.Args[1]
	must(os.Mkdir(out, 0755))
	var signers []*ecdsa.PrivateKey
	for i := 0; i < 4; i++ {
		signer, err := crypto.ToECDSA(crypto.Keccak256([]byte(fmt.Sprintf("public Minter vote fixture %d", i))))
		must(err)
		signers = append(signers, signer)
	}
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	if mode == "--halt" || mode == "--prove-halt" {
		makeHaltFixture(out, now, signers, mode == "--prove-halt")
		return
	}
	if mode == "--commission" {
		makeCommissionFixture(out, now, signers)
		return
	}
	if mode == "--historical-tx" {
		makeHistoricalTransactionFixture(out, now, signers)
		return
	}
	raw, err := tmjson.Marshal(genesis(signers, now))
	must(err)
	cfg := config.GetConfig(filepath.Join(out, "home"))
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	initReq := abci.RequestInitChain{Time: now, ChainId: chainID, InitialHeight: firstHeight, AppStateBytes: raw}
	init := app.InitChain(initReq)
	tree := app.DeliverState().Tree().GetLastImmutable()
	result := fixture{
		Init:        record("InitChain", abci.ToRequestInitChain(initReq), abci.ToResponseInitChain(init)),
		GenesisHash: hex.EncodeToString(tree.Hash()),
	}
	sequence := map[int64][]attempt{
		717: {
			{"v310 second vote", 2, 2, 1, 718, "v310", 0, true},
			{"competing version", 1, 1, 1, 718, "v320", 0, true},
			{"duplicate genesis vote", 0, 0, 1, 718, "v330", 121, true},
			{"duplicate in same block", 2, 2, 2, 718, "v330", 121, false},
			{"invalid version name", 0, 0, 1, 719, "bad-name", 122, true},
			{"expired vote", 0, 0, 1, 716, "v310", 120, true},
			{"sender is not owner", 3, 0, 1, 719, "v310", 406, true},
			{"missing candidate", 3, 77, 1, 719, "v310", 403, true},
			{"v330 future vote", 0, 0, 1, 722, "v330", 0, true},
		},
		719: {{"absent voter proposal", 1, 1, 2, 723, "v999_loser", 0, true}},
		720: {
			{"v320 current height first", 0, 0, 2, 720, "v320", 0, true},
			{"v320 current height second", 1, 1, 3, 720, "v320", 0, true},
		},
		724: {
			{"unknown version first", 0, 0, 3, 724, "v999_local", 0, true},
			{"unknown version second", 2, 2, 2, 724, "v999_local", 0, true},
		},
	}
	var previous []byte
	for height := int64(firstHeight); height <= 724; height++ {
		now = now.Add(5 * time.Second)
		var block = block{Height: height}
		var txs [][]byte
		var mempool sync.Map
		for _, vote := range sequence[height] {
			tx := signedVote(signers[vote.signer], vote)
			txs = append(txs, tx)
			if vote.check {
				block.Exchanges = append(block.Exchanges, checkTx(app, tx, uint64(height), &mempool, vote.label, vote.code))
			}
		}
		if height == 717 || height == 724 {
			must(app.ConfigureCheckpointExport(filepath.Join(out, fmt.Sprint(height))))
		}
		header := tm.Header{ChainID: chainID, Height: height, Time: now, AppHash: previous}
		hash := sha256.Sum256([]byte(fmt.Sprintf("local update vote fixture %d", height)))
		begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
		if height > firstHeight {
			for _, validator := range init.Validators {
				pubkey := ed25519.PubKey(validator.PubKey.GetEd25519())
				voter := pubkey[0] - 1
				present := (height != 722 || voter == 0) && (height != 723 || voter != 1)
				begin.LastCommitInfo.Votes = append(begin.LastCommitInfo.Votes, abci.VoteInfo{
					Validator: abci.Validator{Address: pubkey.Address(), Power: validator.Power}, SignedLastBlock: present,
				})
			}
		}
		block.Exchanges = append(block.Exchanges, record("BeginBlock",
			abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(app.BeginBlock(begin))))
		for i, tx := range txs {
			vote := sequence[height][i]
			block.Exchanges = append(block.Exchanges, deliverTx(app, tx, vote.label, vote.code))
		}
		end := abci.RequestEndBlock{Height: height}
		block.Exchanges = append(block.Exchanges, record("EndBlock",
			abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(app.EndBlock(end))))
		commit := app.Commit()
		block.Exchanges = append(block.Exchanges, record("Commit",
			abci.ToRequestCommit(), abci.ToResponseCommit(commit)))
		previous = commit.Data
		block.AppHash = hex.EncodeToString(previous)
		block.Events, err = tmjson.Marshal(app.GetEventsDB().LoadEvents(uint32(height)))
		must(err)
		for _, version := range app.UpdateVersions() {
			block.Versions = append(block.Versions, mt.Version{Name: version.Name, Height: version.Height})
		}
		result.Blocks = append(result.Blocks, block)
	}
	versions := result.Blocks[len(result.Blocks)-1].Versions
	if len(versions) != 5 || versions[1].Name != "v310" || versions[1].Height != 718 ||
		versions[2].Name != "v320" || versions[2].Height != 720 ||
		versions[3].Name != "v330" || versions[3].Height != 722 ||
		versions[4].Name != "v999_local" || versions[4].Height != 724 {
		panic(fmt.Sprintf("unexpected Go version history: %+v", versions))
	}
	next := tm.Header{ChainID: chainID, Height: 725, Time: now.Add(5 * time.Second), AppHash: previous}
	blocked := abci.RequestBeginBlock{Header: *next.ToProto()}
	if mode == "--prove-stop" {
		app.BeginBlock(blocked)
		panic("Go accepted an unknown upgrade")
	}
	result.Blocked, err = abci.ToRequestBeginBlock(blocked).Marshal()
	must(err)
	must(app.Close())
	result.Pending = snapshot(filepath.Join(out, "717"))
	result.Unknown = snapshot(filepath.Join(out, "724"))
	data, err := json.MarshalIndent(result, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(out, "go-update-votes-v034.json"), append(data, '\n'), 0644))
}

func makeHistoricalTransactionFixture(out string, now time.Time, signers []*ecdsa.PrivateKey) {
	address := func(i int) mt.Address { return crypto.PubkeyToAddress(signers[i].PublicKey) }
	issuer, successor, recipient := address(0), address(1), address(3)
	multisig := accounts.CreateMultisigAddress(issuer, 1)
	state := genesis(signers, now)
	state.Note = "Local Minter historical transactions fixture"
	state.UpdateVotes = nil
	state.Accounts[3].Balance = nil
	state.Accounts = append(state.Accounts, mt.Account{
		Address: multisig, Balance: []mt.Balance{{Value: "100"}},
	})
	symbol := mt.StrToCoinSymbol("AAA")
	state.Coins = []mt.Coin{
		{ID: 1, Name: "First fixture token", Symbol: symbol, Volume: "50000", Reserve: "0", MaxSupply: "100000", OwnerAddress: &issuer, Mintable: true, Burnable: true},
		{ID: 2, Name: "Second fixture token", Symbol: mt.StrToCoinSymbol("BBB"), Volume: "50000", Reserve: "0", MaxSupply: "100000", OwnerAddress: &issuer, Mintable: true, Burnable: true},
	}
	state.Accounts[0].Balance = append(state.Accounts[0].Balance,
		mt.Balance{Coin: 1, Value: "50000"},
		mt.Balance{Coin: 2, Value: "50000"},
	)
	fees := &state.Commission
	fees.Send, fees.CreateMultisig, fees.EditMultisig, fees.EditCandidate, fees.RedeemCheck = "2", "3", "4", "5", "7"
	fees.EditTickerOwner, fees.MintToken, fees.CreateSwapPool = "5", "3", "11"
	fees.BuyPoolBase = "1000"
	must(state.Verify())
	raw, err := tmjson.Marshal(state)
	must(err)
	cfg := config.GetConfig(filepath.Join(out, "home"))
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	initReq := abci.RequestInitChain{Time: now, ChainId: chainID, InitialHeight: firstHeight, AppStateBytes: raw}
	init := app.InitChain(initReq)
	steps := []exchange{record("InitChain", abci.ToRequestInitChain(initReq), abci.ToResponseInitChain(init))}

	signMulti := func(nonce uint64, signer int, operation transaction.Data) []byte {
		data, err := rlp.EncodeToBytes(operation)
		must(err)
		tx := transaction.Transaction{
			Nonce: nonce, ChainID: mt.CurrentChainID, GasPrice: 1,
			Type: operation.TxType(), Data: data, SignatureType: transaction.SigTypeMulti,
		}
		tx.SetMultisigAddress(multisig)
		must(tx.Sign(signers[signer]))
		raw, err := tx.Serialize()
		must(err)
		return raw
	}
	issue := func(nonce string, due uint64, value *big.Int) []byte {
		issued := check.Check{
			Nonce: []byte(nonce), ChainID: mt.CurrentChainID, DueBlock: due, Value: value,
		}
		hash := issued.HashWithoutLock()
		lock, err := crypto.Sign(hash[:], signers[2])
		must(err)
		issued.Lock = new(big.Int).SetBytes(lock)
		must(issued.Sign(signers[0]))
		raw, err := rlp.EncodeToBytes(&issued)
		must(err)
		return raw
	}
	recipientRLP, err := rlp.EncodeToBytes([]mt.Address{recipient})
	must(err)
	proofHash := crypto.Keccak256(recipientRLP)
	validProof, err := crypto.Sign(proofHash, signers[2])
	must(err)
	invalidProof, err := crypto.Sign(proofHash, signers[1])
	must(err)
	// The Go proof verifier also accepts the high-S representation.
	highS := new(big.Int).SetBytes(validProof[32:64])
	highS.Sub(crypto.S256().Params().N, highS).FillBytes(validProof[32:64])
	validProof[64] ^= 1
	redeem := func(nonce uint64, raw, proof []byte) []byte {
		var compact [65]byte
		copy(compact[:], proof)
		return signedTransaction(signers[3], nonce, transaction.RedeemCheckData{RawCheck: raw, Proof: compact})
	}
	issued := issue("once", 718, big.NewInt(100))
	expired := redeem(1, issue("expired", 716, big.NewInt(100)), validProof)
	unfunded := redeem(1, issue("unfunded", 717, helpers.BipToPip(big.NewInt(20000))), validProof)
	empty := redeem(1, nil, validProof)
	badProof := redeem(1, issued, invalidProof)
	good := redeem(1, issued, validProof)
	duplicate := redeem(2, issued, validProof)
	create := signedTransaction(signers[0], 1, transaction.CreateMultisigData{
		Threshold: 1, Weights: []uint32{1, 1}, Addresses: []mt.Address{issuer, successor},
	})
	edit := signMulti(1, 0, transaction.EditMultisigData{
		Threshold: 2, Weights: []uint32{2}, Addresses: []mt.Address{successor},
	})
	send := transaction.SendData{To: recipient, Value: big.NewInt(10)}
	editCandidate := func(signer int, nonce uint64, reward mt.Address) []byte {
		return signedTransaction(signers[signer], nonce, transaction.EditCandidateData{
			PubKey: key(0), OwnerAddress: successor, RewardAddress: reward, ControlAddress: recipient,
		})
	}
	transferTicker := signedTransaction(signers[0], 3, transaction.EditCoinOwnerData{
		Symbol: symbol, NewOwner: successor,
	})
	mint := transaction.MintTokenData{Coin: 1, Value: big.NewInt(1)}
	createPool := func(nonce uint64, coin0, coin1 mt.CoinID, volume0, volume1 int64) []byte {
		return signedTransaction(signers[0], nonce, transaction.CreateSwapPoolData{
			Coin0: coin0, Coin1: coin1, Volume0: big.NewInt(volume0), Volume1: big.NewInt(volume1),
		})
	}
	reverseQuote := signedTransactionInCoin(signers[0], 6, 1, transaction.BuySwapPoolDataV260{
		Coins: []mt.CoinID{0, 1}, ValueToBuy: big.NewInt(500), MaximumValueToSell: big.NewInt(1),
	})

	var previous []byte
	for height := int64(firstHeight); height <= 718; height++ {
		var mempool sync.Map
		if height == 717 {
			steps = append(steps,
				checkTx(app, empty, uint64(height), &mempool, "empty check", code.DecodeError),
				checkTx(app, badProof, uint64(height), &mempool, "wrong proof", code.CheckInvalidLock),
				checkTx(app, unfunded, uint64(height), &mempool, "insufficient issuer balance", code.InsufficientFunds),
				checkTx(app, good, uint64(height), &mempool, "recipient has zero balance", code.OK))
		}
		if height == 718 {
			steps = append(steps, checkTx(app, duplicate, uint64(height), &mempool, "persisted used check", code.CheckUsed))
			steps = append(steps, checkTx(app, reverseQuote, uint64(height), &mempool, "reverse fee pool quote", code.MaximumValueToSellReached))
		}
		now = now.Add(5 * time.Second)
		header := tm.Header{ChainID: chainID, Height: height, Time: now, AppHash: previous}
		begin := abci.RequestBeginBlock{Header: *header.ToProto()}
		if height > firstHeight {
			for _, validator := range init.Validators {
				key := ed25519.PubKey(validator.PubKey.GetEd25519())
				begin.LastCommitInfo.Votes = append(begin.LastCommitInfo.Votes, abci.VoteInfo{
					Validator: abci.Validator{Address: key.Address(), Power: validator.Power}, SignedLastBlock: true,
				})
			}
		}
		steps = append(steps, record("BeginBlock", abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(app.BeginBlock(begin))))
		if height == 717 {
			steps = append(steps,
				deliverTx(app, empty, "empty check", code.DecodeError),
				deliverTx(app, expired, "expired check", code.CheckExpired),
				deliverTx(app, badProof, "wrong proof", code.CheckInvalidLock),
				deliverTx(app, good, "issuer pays for redemption", code.OK),
				deliverTx(app, duplicate, "duplicate in the same block", code.CheckUsed),
				deliverTx(app, create, "create from a funded address", code.OK),
				deliverTx(app, edit, "replace signers in the same block", code.OK),
				deliverTx(app, signMulti(2, 0, send), "old signer", code.NotEnoughMultisigVotes),
				deliverTx(app, signMulti(2, 1, send), "new signer", code.OK),
				deliverTx(app, editCandidate(0, 2, issuer), "change candidate owner", code.OK),
				deliverTx(app, editCandidate(0, 3, issuer), "old owner", code.IsNotOwnerOfCandidate),
				deliverTx(app, editCandidate(1, 1, successor), "new owner", code.OK),
				deliverTx(app, transferTicker, "transfer the ticker", code.OK),
				deliverTx(app, signedTransaction(signers[0], 4, mint), "old ticker owner", code.IsNotOwnerOfCoin),
				deliverTx(app, signedTransaction(signers[1], 2, mint), "new ticker owner", code.OK),
				deliverTx(app, createPool(4, 1, 0, 1000, 1000), "liquidity at the locked minimum", code.InsufficientLiquidityMinted),
				deliverTx(app, createPool(4, 1, 0, 10000, 20000), "first pool in reverse coin order", code.OK),
				deliverTx(app, createPool(5, 0, 1, 20000, 10000), "duplicate pool in the same block", code.PairAlreadyExists),
				deliverTx(app, createPool(5, 2, 1, 8000, 10000), "second pool in the same block", code.OK))
		}
		if height == 718 {
			steps = append(steps,
				deliverTx(app, duplicate, "persisted used check", code.CheckUsed),
				deliverTx(app, reverseQuote, "reverse fee pool quote", code.MaximumValueToSellReached),
				deliverTx(app, createPool(6, 0, 1, 20000, 10000), "persisted pool", code.PairAlreadyExists),
				deliverTx(app, createPool(6, 2, 0, 8000, 10000), "next pool after reopen", code.OK))
		}
		end := abci.RequestEndBlock{Height: height}
		steps = append(steps, record("EndBlock", abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(app.EndBlock(end))))
		commit := app.Commit()
		previous = commit.Data
		steps = append(steps, record(fmt.Sprintf("Commit %d", height), abci.ToRequestCommit(), abci.ToResponseCommit(commit)))
	}
	must(app.Close())
	data, err := json.MarshalIndent(steps, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(out, "go-historical-transactions-v034.json"), append(data, '\n'), 0644))
}

type haltFixture struct {
	Init        exchange   `json:"init"`
	GenesisHash string     `json:"genesisHash"`
	Blocks      []block    `json:"blocks"`
	Pending     checkpoint `json:"pending"`
	Blocked     []byte     `json:"blocked"`
}

type haltAttempt struct {
	label  string
	signer int
	voter  int
	nonce  uint64
	height uint64
	code   uint32
	check  bool
}

func makeHaltFixture(out string, now time.Time, signers []*ecdsa.PrivateKey, proveStop bool) {
	state := genesis(signers, now)
	state.Note = "Local Minter halt vote fixture"
	state.UpdateVotes = nil
	state.Commission.SetHaltBlock = "7"
	// Go parses these exported records and omits them from State.Import.
	state.HaltBlocks = []mt.HaltBlock{{Height: 717, CandidateKey: key(0)}, {Height: 717, CandidateKey: key(1)}}
	raw, err := tmjson.Marshal(state)
	must(err)
	cfg := config.GetConfig(filepath.Join(out, "home"))
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	initReq := abci.RequestInitChain{Time: now, ChainId: chainID, InitialHeight: firstHeight, AppStateBytes: raw}
	init := app.InitChain(initReq)
	tree := app.DeliverState().Tree().GetLastImmutable()
	result := haltFixture{
		Init:        record("InitChain", abci.ToRequestInitChain(initReq), abci.ToResponseInitChain(init)),
		GenesisHash: hex.EncodeToString(tree.Hash()),
	}
	sequence := map[int64][]haltAttempt{
		717: {
			{"first future halt vote", 0, 0, 1, 722, 0, true},
			{"first vote for 719", 1, 1, 1, 719, 0, true},
			{"only vote for 718", 2, 2, 1, 718, 0, true},
			{"second vote for 719", 0, 0, 2, 719, 0, false},
			{"same-block duplicate", 0, 0, 3, 719, 118, false},
			{"expired height", 3, 0, 1, 716, 120, true},
			{"missing candidate", 3, 77, 1, 721, 403, true},
			{"sender is not owner", 3, 0, 1, 721, 406, true},
		},
		718: {
			{"duplicate vote marked for removal", 2, 2, 2, 718, 118, true},
			{"vote added to the expired record", 1, 1, 2, 718, 0, true},
		},
		720: {
			{"second future halt vote", 1, 1, 3, 722, 0, true},
			{"same-height vote without an existing record", 2, 2, 2, 720, 0, true},
		},
	}
	beginRequest := func(height int64, previous []byte) abci.RequestBeginBlock {
		header := tm.Header{ChainID: chainID, Height: height, Time: now, AppHash: previous}
		hash := sha256.Sum256([]byte(fmt.Sprintf("local halt vote fixture %d", height)))
		begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
		if height > firstHeight {
			for _, validator := range init.Validators {
				pubkey := ed25519.PubKey(validator.PubKey.GetEd25519())
				begin.LastCommitInfo.Votes = append(begin.LastCommitInfo.Votes, abci.VoteInfo{
					Validator:       abci.Validator{Address: pubkey.Address(), Power: validator.Power},
					SignedLastBlock: height != 719 || pubkey[0] != 2,
				})
			}
		}
		return begin
	}
	var previous []byte
	for height := int64(firstHeight); height <= 721; height++ {
		now = now.Add(5 * time.Second)
		var block = block{Height: height}
		var txs [][]byte
		var mempool sync.Map
		for _, vote := range sequence[height] {
			tx := signedTransaction(signers[vote.signer], vote.nonce,
				transaction.SetHaltBlockData{PubKey: key(vote.voter), Height: vote.height})
			txs = append(txs, tx)
			if vote.check {
				block.Exchanges = append(block.Exchanges, checkTx(app, tx, uint64(height), &mempool, vote.label, vote.code))
			}
		}
		if height == 717 {
			must(app.ConfigureCheckpointExport(filepath.Join(out, "717")))
		}
		begin := beginRequest(height, previous)
		block.Exchanges = append(block.Exchanges, record("BeginBlock",
			abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(app.BeginBlock(begin))))
		for i, tx := range txs {
			vote := sequence[height][i]
			block.Exchanges = append(block.Exchanges, deliverTx(app, tx, vote.label, vote.code))
		}
		end := abci.RequestEndBlock{Height: height}
		block.Exchanges = append(block.Exchanges, record("EndBlock",
			abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(app.EndBlock(end))))
		commit := app.Commit()
		block.Exchanges = append(block.Exchanges, record("Commit",
			abci.ToRequestCommit(), abci.ToResponseCommit(commit)))
		previous = commit.Data
		block.AppHash = hex.EncodeToString(previous)
		result.Blocks = append(result.Blocks, block)
	}
	for height, expected := range map[uint64]bool{717: false, 718: false, 719: false, 720: true, 722: true} {
		path := make([]byte, 9)
		path[0] = 'h'
		binary.LittleEndian.PutUint64(path[1:], height)
		_, value := app.DeliverState().Tree().GetLastImmutable().Get(path)
		if (len(value) != 0) != expected {
			panic(fmt.Sprintf("unexpected halt row at %d: %x", height, value))
		}
	}
	now = now.Add(5 * time.Second)
	blocked := beginRequest(722, previous)
	if proveStop {
		app.BeginBlock(blocked)
		panic("Go accepted a block with a halt quorum")
	}
	result.Blocked, err = abci.ToRequestBeginBlock(blocked).Marshal()
	must(err)
	must(app.Close())
	result.Pending = snapshot(filepath.Join(out, "717"))
	data, err := json.MarshalIndent(result, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(out, "go-halt-votes-v034.json"), append(data, '\n'), 0644))
}

type commissionFixture struct {
	Init        exchange   `json:"init"`
	GenesisHash string     `json:"genesisHash"`
	Blocks      []block    `json:"blocks"`
	Pending     checkpoint `json:"pending"`
}

func commissionVote(voter int, height uint64, price mt.Commission) transaction.VoteCommissionDataV3 {
	vote := transaction.VoteCommissionDataV3{PubKey: key(voter), Height: height, Coin: mt.CoinID(price.Coin)}
	fields, source := reflect.ValueOf(&vote).Elem(), reflect.ValueOf(price)
	for i := 0; i < fields.NumField(); i++ {
		field := fields.Field(i)
		if field.Type() != reflect.TypeOf((*big.Int)(nil)) {
			continue
		}
		name := fields.Type().Field(i).Name
		if name == "CreateTicker7to10" {
			name = "CreateTicker7_10"
		}
		field.Set(reflect.ValueOf(helpers.StringToBigInt(source.FieldByName(name).String())))
	}
	return vote
}

func makeCommissionFixture(out string, now time.Time, signers []*ecdsa.PrivateKey) {
	state := genesis(signers, now)
	state.Note = "Local Minter commission vote fixture"
	state.UpdateVotes = nil
	state.Commission.Send, state.Commission.VoteCommission = "3", "9"
	base := state.Commission
	base.Send, base.VoteCommission, base.BurnToken, base.MintToken = "11", "13", "29", "31"
	pooled := base
	pooled.Coin, pooled.Send, pooled.FailedTx = 1, "17", "19"
	state.CommissionVotes = []mt.CommissionVote{{Height: 718, Votes: []mt.Pubkey{key(0)}, Commission: base}}
	token := helpers.BipToPip(big.NewInt(20000)).String()
	bip := helpers.BipToPip(big.NewInt(100000)).String()
	state.Coins = []mt.Coin{
		{ID: 1, Name: "Fixture fee token", Symbol: mt.StrToCoinSymbol("FEE"), Volume: token, Reserve: "0", MaxSupply: bip},
		{ID: 2, Name: "Fixture token without pool", Symbol: mt.StrToCoinSymbol("NOP"), Volume: "0", Reserve: "0", MaxSupply: bip},
	}
	state.Pools = []mt.Pool{{Coin0: 0, Coin1: 1, Reserve0: bip, Reserve1: token, ID: 1}}
	must(state.Verify())
	raw, err := tmjson.Marshal(state)
	must(err)
	cfg := config.GetConfig(filepath.Join(out, "home"))
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	initReq := abci.RequestInitChain{Time: now, ChainId: chainID, InitialHeight: firstHeight, AppStateBytes: raw}
	init := app.InitChain(initReq)
	result := commissionFixture{
		Init:        record("InitChain", abci.ToRequestInitChain(initReq), abci.ToResponseInitChain(init)),
		GenesisHash: hex.EncodeToString(app.DeliverState().Tree().GetLastImmutable().Hash()),
	}
	type attempt struct {
		label  string
		signer int
		nonce  uint64
		data   transaction.Data
		code   uint32
		check  bool
	}
	to := crypto.PubkeyToAddress(signers[0].PublicKey)
	send := transaction.SendData{To: to, Value: big.NewInt(25)}
	unknownCoin, noPool, extra := commissionVote(0, 718, base), commissionVote(0, 718, base), commissionVote(0, 718, base)
	unknownCoin.Coin, noPool.Coin, extra.More = 99, 2, []*big.Int{big.NewInt(1)}
	// These proposals must use a fresh height to reach coin and tail validation.
	unknownCoin.Height, noPool.Height, extra.Height = 721, 721, 721
	sequence := map[int64][]attempt{
		717: {
			{"duplicate genesis commission vote", 0, 1, commissionVote(0, 718, pooled), 121, true},
			{"second vote for BIP schedule", 1, 1, commissionVote(1, 718, base), 0, true},
			{"competing pooled schedule", 2, 1, commissionVote(2, 718, pooled), 0, true},
			{"single vote for next height", 2, 2, commissionVote(2, 719, pooled), 0, false},
			{"unknown price coin", 3, 1, unknownCoin, 102, true},
			{"price coin without BIP pool", 3, 1, noPool, 701, true},
			{"extra price", 3, 1, extra, 106, true},
			{"expired commission vote", 3, 1, commissionVote(0, 716, base), 120, true},
			{"missing candidate", 3, 1, commissionVote(77, 721, base), 403, true},
			{"sender is not owner", 3, 1, commissionVote(0, 721, base), 406, true},
		},
		718: {{"old BIP fee", 3, 1, send, 0, true}},
		719: {{"updated BIP fee", 3, 2, send, 0, true}},
		720: {
			{"current-height pooled vote", 0, 1, commissionVote(0, 720, pooled), 0, true},
			{"second current-height pooled vote", 1, 2, commissionVote(1, 720, pooled), 0, true},
			{"first network update vote", 0, 2, transaction.VoteUpdateDataV230{Version: minter.V310, PubKey: key(0), Height: 720}, 0, false},
			{"second network update vote", 1, 3, transaction.VoteUpdateDataV230{Version: minter.V310, PubKey: key(1), Height: 720}, 0, false},
		},
		721: {
			{"converted fee", 3, 3, send, 0, true},
			{"converted failed fee", 3, 4, commissionVote(0, 720, base), 120, false},
		},
	}
	var previous []byte
	for height := int64(firstHeight); height <= 721; height++ {
		now = now.Add(5 * time.Second)
		var block = block{Height: height}
		var txs [][]byte
		var mempool sync.Map
		for _, tx := range sequence[height] {
			raw := signedTransaction(signers[tx.signer], tx.nonce, tx.data)
			txs = append(txs, raw)
			if tx.check {
				block.Exchanges = append(block.Exchanges, checkTx(app, raw, uint64(height), &mempool, tx.label, tx.code))
			}
		}
		if height == 717 {
			must(app.ConfigureCheckpointExport(filepath.Join(out, "717")))
		}
		header := tm.Header{ChainID: chainID, Height: height, Time: now, AppHash: previous}
		hash := sha256.Sum256([]byte(fmt.Sprintf("local commission vote fixture %d", height)))
		begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
		if height > firstHeight {
			for _, validator := range init.Validators {
				pubkey := ed25519.PubKey(validator.PubKey.GetEd25519())
				begin.LastCommitInfo.Votes = append(begin.LastCommitInfo.Votes, abci.VoteInfo{
					Validator:       abci.Validator{Address: pubkey.Address(), Power: validator.Power},
					SignedLastBlock: true,
				})
			}
		}
		block.Exchanges = append(block.Exchanges, record("BeginBlock",
			abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(app.BeginBlock(begin))))
		for i, tx := range txs {
			attempt := sequence[height][i]
			block.Exchanges = append(block.Exchanges, deliverTx(app, tx, attempt.label, attempt.code))
		}
		end := abci.RequestEndBlock{Height: height}
		block.Exchanges = append(block.Exchanges, record("EndBlock",
			abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(app.EndBlock(end))))
		commit := app.Commit()
		block.Exchanges = append(block.Exchanges, record("Commit",
			abci.ToRequestCommit(), abci.ToResponseCommit(commit)))
		previous = commit.Data
		block.AppHash = hex.EncodeToString(previous)
		block.Events, err = tmjson.Marshal(app.GetEventsDB().LoadEvents(uint32(height)))
		must(err)
		for _, version := range app.UpdateVersions() {
			block.Versions = append(block.Versions, mt.Version{Name: version.Name, Height: version.Height})
		}
		result.Blocks = append(result.Blocks, block)
	}
	price := app.CurrentState().Commission().GetCommissions()
	if price.Coin != 1 || price.Send.String() != "17" || price.MintToken.String() != "31" || price.BurnToken.String() != "29" {
		panic("Go did not apply the expected pooled fee schedule")
	}
	must(app.Close())
	result.Pending = snapshot(filepath.Join(out, "717"))
	data, err := json.MarshalIndent(result, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(out, "go-commission-votes-v034.json"), append(data, '\n'), 0644))
}
