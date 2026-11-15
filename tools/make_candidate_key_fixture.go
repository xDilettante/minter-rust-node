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
	app           *minter.Blockchain
	signer        *ecdsa.PrivateKey
	dir           string
	name          string
	checkpoint    string
	at            time.Time
	root          []byte
	votes         []abci.VoteInfo
	blocks        []block
	initialize    []exchange
	checks        []exchange
	partial       []exchange
	nextEvidence  []uint32
	frozenHeights []uint64
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
	for _, id := range o.nextEvidence {
		public := key(id)
		begin.ByzantineValidators = append(begin.ByzantineValidators, abci.Evidence{
			Type:      abci.EvidenceType_DUPLICATE_VOTE,
			Validator: abci.Validator{Address: ed25519.PubKey(public[:]).Address(), Power: 1},
			Height:    height - 1, Time: o.at,
		})
	}
	o.nextEvidence = nil
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
	o.partial = b.Exchanges
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
	for _, due := range o.frozenHeights {
		_, value := tree.Get(binary.BigEndian.AppendUint64([]byte{'f'}, due))
		o.blocks[len(o.blocks)-1].Readings = append(o.blocks[len(o.blocks)-1].Readings,
			reading{binary.BigEndian.AppendUint64([]byte{'f'}, due), value})
	}
}

func (o *oracle) checkTx(data transaction.Data, code uint32) {
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

func publicKeyGenesis(owner mt.Address, at time.Time) mt.AppState {
	g := seed(owner, at)
	g.Commission.EditCandidatePublicKey = "5"
	g.BlockListCandidates = []mt.Pubkey{key(990)}
	for i := range g.Candidates {
		c := &g.Candidates[i]
		if c.ID == 253 || c.ID == 255 || c.ID == 256 || c.ID == 97 || c.ID == 96 {
			c.OwnerAddress, c.ControlAddress = owner, owner
		}
		if c.ID == 1 {
			c.ControlAddress = owner
		}
	}
	return g
}

func rotate(from, to uint32, code uint32) action {
	return action{data: transaction.EditCandidatePublicKeyData{PubKey: key(from), NewPubKey: key(to)}, code: code}
}

func main() {
	if len(os.Args) != 2 && !(len(os.Args) == 3 && (os.Args[2] == "--active-payout" || os.Args[2] == "--frozen-keys")) {
		panic("usage: make_candidate_key_fixture NEW_OUTPUT_DIRECTORY [--active-payout]")
	}
	dir := os.Args[1]
	check(os.Mkdir(dir, 0755))
	signer, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter candidate fixture wallet")))
	check(err)
	owner := crypto.PubkeyToAddress(signer.PublicKey)
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	g := publicKeyGenesis(owner, now)
	if len(os.Args) == 3 && os.Args[2] == "--frozen-keys" {
		frozenKeys(dir, g, signer, now)
		return
	}
	if len(os.Args) == 3 {
		payout := start(dir, "candidate-key-active-payout", g, signer, now, 719)
		defer func() {
			if failure := recover(); failure != nil {
				info := payout.app.Info(abci.RequestInfo{})
				control := start(dir, "candidate-key-payout-control", g, signer, now, 719)
				control.execute(720, nil)
				request, err := abci.ToRequestEndBlock(abci.RequestEndBlock{Height: 720}).Marshal()
				check(err)
				data, err := json.MarshalIndent(struct {
					Initial         scenario          `json:"initial"`
					Exchanges       []exchange        `json:"exchanges"`
					EndBlockRequest []byte            `json:"endBlockRequest"`
					Panic           string            `json:"panic"`
					ReportedHeight  uint64            `json:"reportedHeight"`
					Info            abci.ResponseInfo `json:"info"`
					Control         scenario          `json:"control"`
				}{payout.finish(), payout.partial, request, fmt.Sprint(failure), payout.app.Height(), info, control.finish()}, "", "  ")
				check(err)
				check(os.WriteFile(filepath.Join(dir, "go-active-payout-failure.json"), append(data, '\n'), 0644))
				panic(failure)
			}
		}()
		payout.execute(720, []action{rotate(253, 8000, 0)})
		return
	}
	app := start(dir, "candidate-key-sequence", g, signer, now, 717)
	app.execute(718, []action{
		rotate(999, 7000, 403), rotate(1, 7000, 406), rotate(255, 255, 411), rotate(255, 256, 401),
		rotate(255, 990, 410), rotate(255, 7000, 0), rotate(255, 7001, 403),
		rotate(7000, 7001, 0), rotate(7000, 7002, 403), rotate(7001, 255, 410), rotate(256, 7001, 401),
		rotate(253, 8000, 0),
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(96), NewPubKey: mt.Pubkey{}}},
		{data: transaction.EditCandidatePublicKeyData{PubKey: mt.Pubkey{}, NewPubKey: key(7300)}},
		{data: transaction.EditCandidatePublicKeyData{PubKey: mt.Pubkey{}, NewPubKey: key(7301)}, code: 403},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(256), NewPubKey: mt.Pubkey{}}, code: 410},
	})
	app.execute(719, []action{
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(7300), NewPubKey: mt.Pubkey{}}, code: 410},
		rotate(7001, 7002, 0), rotate(7002, 7000, 410), rotate(253, 8001, 403),
		{data: transaction.DelegateData{PubKey: key(7002), Coin: 0, Value: bip(1)}},
		{data: transaction.DelegateData{PubKey: key(255), Coin: 0, Value: bip(1)}, code: 403},
	})
	app.execute(720, []action{rotate(7002, 7003, 0)})
	app.execute(721, []action{rotate(8000, 8001, 0), rotate(7003, 8001, 401)})
	exported := app.app.DeliverState().Export()
	exported.Version = minter.V3
	exported.PrevReward = g.PrevReward
	exported.Emission = app.app.GetEmission().String()
	scenarios := []scenario{app.finish()}
	second := start(dir, "candidate-key-after-import", exported, signer, now, 722)
	second.execute(723, []action{rotate(7003, 255, 410), rotate(7003, 7000, 410), rotate(7003, 7004, 0), rotate(8001, 8002, 0)})
	second.execute(724, []action{rotate(7004, 7003, 410), rotate(8002, 8001, 410)})
	scenarios = append(scenarios, second.finish())

	poorSigner, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter candidate key poor wallet")))
	check(err)
	poor := crypto.PubkeyToAddress(poorSigner.PublicKey)
	fees := publicKeyGenesis(owner, now)
	fees.Coins[0].Reserve = bip(20_000).String()
	fees.Accounts = append(fees.Accounts, mt.Account{Address: poor, Balance: []mt.Balance{{Coin: 0, Value: bip(1).String()}}})
	for i := range fees.Candidates {
		if fees.Candidates[i].ID == 97 {
			fees.Candidates[i].OwnerAddress = poor
		}
	}
	feeApp := start(dir, "candidate-key-gas-coins", fees, signer, now, 700)
	feeApp.execute(701, []action{{data: transaction.CreateSwapPoolData{Coin0: 0, Coin1: 2, Volume0: bip(10), Volume1: new(big.Int).Div(bip(1), big.NewInt(2))}}})
	feeApp.execute(702, []action{
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(255), NewPubKey: key(7100)}, gasCoin: 1},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(256), NewPubKey: key(7101)}, gasCoin: 2},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(97), NewPubKey: key(990)}, gasCoin: 2, signer: poorSigner, code: 107},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(97), NewPubKey: key(7101)}, gasCoin: 2, signer: poorSigner, code: 401},
		{data: transaction.SendData{Coin: 2, To: poor, Value: new(big.Int).Div(bip(1), big.NewInt(10))}},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(97), NewPubKey: key(990)}, gasCoin: 2, signer: poorSigner, code: 410},
		{data: transaction.EditCandidatePublicKeyData{PubKey: key(97), NewPubKey: key(7102)}, gasCoin: 2, signer: poorSigner},
	})
	feeApp.execute(703, []action{rotate(7100, 255, 410), rotate(7101, 256, 410), rotate(7100, 7103, 0)})
	scenarios = append(scenarios, feeApp.finish())
	checked := start(dir, "candidate-key-checktx", publicKeyGenesis(owner, now), signer, now, 700)
	checked.checkTx(transaction.EditCandidatePublicKeyData{PubKey: key(255), NewPubKey: key(7200)}, 0)
	checked.execute(701, []action{rotate(255, 7200, 0)})
	checked.checkTx(transaction.EditCandidatePublicKeyData{PubKey: key(255), NewPubKey: key(7201)}, 403)
	checked.execute(702, []action{rotate(255, 7201, 403), rotate(7200, 255, 410)})
	scenarios = append(scenarios, checked.finish())
	data, err := json.MarshalIndent(scenarios, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-candidate-key-v034.json"), append(data, '\n'), 0644))
}

func frozenKeys(dir string, g mt.AppState, signer *ecdsa.PrivateKey, now time.Time) {
	mt.CurrentChainID = mt.ChainTestnet
	kept := g.Candidates[:0]
	g.Validators = nil
	for _, candidate := range g.Candidates {
		if candidate.ID != 253 && candidate.ID != 255 {
			continue
		}
		candidate.Status = 2
		kept = append(kept, candidate)
		g.Validators = append(g.Validators, mt.Validator{PubKey: candidate.PubKey, TotalBipStake: candidate.TotalBipStake,
			AccumReward: "0", AbsentTimes: mt.NewBitArray(24)})
	}
	g.Candidates = kept
	g.Waitlist = nil
	due := uint64(701 + mt.GetUnbondPeriod())
	original := key(253)
	g.FrozenFunds = []mt.FrozenFund{{Height: due, Address: crypto.PubkeyToAddress(signer.PublicKey),
		CandidateKey: &original, CandidateID: 253, Coin: 0, Value: bip(1).String()}}
	app := start(dir, "candidate-key-frozen-references", g, signer, now, 700)
	app.frozenHeights = []uint64{due}
	app.nextEvidence = []uint32{253}
	app.execute(701, []action{rotate(253, 9000, 0)})
	app.execute(702, []action{rotate(9000, 9001, 0)})
	logical := func() []string {
		var result []string
		for _, item := range app.app.DeliverState().FrozenFunds.GetFrozenFunds(due).List {
			if item.CandidateKey == nil {
				result = append(result, "")
			} else {
				result = append(result, item.CandidateKey.String())
			}
		}
		return result
	}
	observations := map[string][]string{"after702": logical()}
	app.execute(703, []action{{data: transaction.LockData{DueBlock: uint32(due), Coin: 0, Value: bip(1)}}})
	observations["after703"] = logical()
	app.execute(704, []action{rotate(9001, 9002, 0)})
	observations["after704"] = logical()
	exported := app.app.DeliverState().Export()
	exported.Version = minter.V3
	exported.PrevReward = g.PrevReward
	exported.Emission = app.app.GetEmission().String()
	cold := start(dir, "candidate-key-frozen-after-import", exported, signer, now, int64(due)-1)
	cold.frozenHeights = []uint64{due}
	for _, item := range cold.app.DeliverState().FrozenFunds.GetFrozenFunds(due).List {
		if item.CandidateKey == nil {
			observations["afterImport"] = append(observations["afterImport"], "")
		} else {
			observations["afterImport"] = append(observations["afterImport"], item.CandidateKey.String())
		}
	}
	cold.execute(int64(due), []action{rotate(9002, 9003, 0)})
	cold.execute(int64(due)+1, nil)
	for height := int64(705); height < int64(due); height++ {
		app.execute(height, nil)
	}
	app.execute(int64(due), []action{rotate(9002, 9003, 0)})
	app.execute(int64(due)+1, nil)
	data, err := json.MarshalIndent([]scenario{app.finish(), cold.finish()}, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-candidate-key-frozen-v034.json"), append(data, '\n'), 0644))
	data, err = json.MarshalIndent(observations, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "logical-frozen-keys.json"), append(data, '\n'), 0644))
}
