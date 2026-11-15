// Run with go run -mod=readonly from the pinned Minter Go module.
package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"reflect"
	"time"

	"github.com/MinterTeam/minter-go-node/cmd/utils"
	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/dao"
	"github.com/MinterTeam/minter-go-node/coreV2/developers"
	"github.com/MinterTeam/minter-go-node/coreV2/minter"
	"github.com/MinterTeam/minter-go-node/coreV2/rewards"
	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/helpers"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	tmjson "github.com/tendermint/tendermint/libs/json"
	"github.com/tendermint/tendermint/libs/log"
	tm "github.com/tendermint/tendermint/types"
)

const payoutHeight = 10_523_520
const chainID = "minter-mainnet-5"

type scenario struct {
	name       string
	v310       int64
	v320       int64
	v330       int64
	vote       bool
	correction bool
	cap        bool
}

type exchange struct {
	Request  []byte `json:"request"`
	Response []byte `json:"response"`
}

type block struct {
	Height   int64           `json:"height"`
	Begin    exchange        `json:"begin"`
	End      exchange        `json:"end"`
	Commit   exchange        `json:"commit"`
	AppHash  string          `json:"appHash"`
	Emission string          `json:"emission"`
	Events   json.RawMessage `json:"events"`
}

type fixture struct {
	Name     string          `json:"name"`
	Manifest json.RawMessage `json:"manifest"`
	Snapshot []byte          `json:"snapshot"`
	Blocks   []block         `json:"blocks"`
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func record(req *abci.Request, res *abci.Response) exchange {
	request, err := req.Marshal()
	must(err)
	response, err := res.Marshal()
	must(err)
	return exchange{request, response}
}

func genesis(s scenario, now time.Time) mt.AppState {
	var prices mt.Commission
	fields := reflect.ValueOf(&prices).Elem()
	for i := 0; i < fields.NumField(); i++ {
		if field := fields.Field(i); field.Kind() == reflect.String {
			field.SetString("0")
		}
	}
	g := mt.AppState{
		Note:    "Local payout fixture from the Go application",
		Version: minter.V3, Versions: []mt.Version{{Name: minter.V3, Height: 10_196_999}},
		Commission: prices, Emission: "1000000000000000000000000000", TotalSlashed: "0", MaxGas: 90000,
		PrevReward: mt.RewardPrice{Time: uint64(now.UnixNano()), AmountBIP: "1", AmountUSDT: "1", Reward: "123456789013"},
	}
	for _, version := range []struct {
		name   string
		offset int64
	}{{minter.V310, s.v310}, {minter.V320, s.v320}, {minter.V330, s.v330}} {
		if version.offset != 0 {
			g.Versions = append(g.Versions, mt.Version{Name: version.name, Height: uint64(payoutHeight + version.offset)})
		}
	}
	if s.cap {
		g.Emission = rewards.TotalEmission
	}
	for i := byte(1); i <= 2; i++ {
		key := mt.Pubkey{i}
		if s.correction && i == 1 {
			raw, err := hex.DecodeString("7771ad434bd6df3519ee5bd5331e66815263a3d8afbdd6768003ff7f790db6bc")
			must(err)
			copy(key[:], raw)
		}
		owner, other := mt.Address{i, 1}, mt.Address{i, 2}
		g.Accounts = append(g.Accounts, mt.Account{Address: owner, LockStakeUntilBlock: payoutHeight + 2})
		g.Accounts = append(g.Accounts, mt.Account{Address: other, LockStakeUntilBlock: payoutHeight})
		candidate := mt.Candidate{
			ID: uint64(i), PubKey: key, Status: 2, Commission: uint64(i) * 13,
			OwnerAddress: owner, RewardAddress: owner, ControlAddress: owner,
		}
		total := big.NewInt(0)
		addStake := func(address mt.Address, value string) {
			total.Add(total, helpers.StringToBigInt(value))
			candidate.Stakes = append(candidate.Stakes, mt.Stake{Owner: address, Value: value, BipValue: value})
		}
		addStake(owner, helpers.BipToPip(big.NewInt(int64(i)*1371)).String())
		addStake(other, "234567890123456789019")
		if s.correction && i == 1 {
			addStake(dao.Address, "4784167411444000956")
			addStake(developers.Address, "4786423506308566880")
		}
		candidate.TotalBipStake = total.String()
		g.Candidates = append(g.Candidates, candidate)
		reward := "0"
		if s.cap && i == 1 {
			reward = "98765432103"
		}
		g.Validators = append(g.Validators, mt.Validator{
			PubKey: key, TotalBipStake: candidate.TotalBipStake, AccumReward: reward, AbsentTimes: mt.NewBitArray(24),
		})
	}
	must(g.Verify())
	return g
}

func generate(dir string, s scenario) fixture {
	must(os.Mkdir(dir, 0755))
	cfg := config.GetConfig(filepath.Join(dir, "home"))
	cfg.DBBackend = "memdb"
	app := minter.NewMinterBlockchain(utils.NewStorage(cfg.RootDir, ""), cfg, context.Background(), 0, 0, log.NewNopLogger())
	now := time.Date(2025, 1, 1, 8, 0, 0, 0, time.UTC)
	g := genesis(s, now)
	raw, err := tmjson.Marshal(g)
	must(err)
	init := app.InitChain(abci.RequestInitChain{Time: now, ChainId: chainID, InitialHeight: payoutHeight - 1, AppStateBytes: raw})
	must(app.ConfigureCheckpointExport(filepath.Join(dir, "checkpoint")))
	var votes []abci.VoteInfo
	for _, validator := range init.Validators {
		key := ed25519.PubKey(validator.PubKey.GetEd25519())
		signed := s.vote && key.Equals(ed25519.PubKey(g.Validators[0].PubKey.Bytes()))
		votes = append(votes, abci.VoteInfo{
			Validator: abci.Validator{Address: key.Address(), Power: validator.Power}, SignedLastBlock: signed,
		})
	}
	var trace []block
	var previous []byte
	for height := int64(payoutHeight - 1); height <= payoutHeight+1; height++ {
		now = now.Add(5 * time.Second)
		header := tm.Header{ChainID: chainID, Height: height, Time: now, AppHash: previous}
		hash := sha256.Sum256([]byte(fmt.Sprintf("local reward fixture %s %d", s.name, height)))
		begin := abci.RequestBeginBlock{Header: *header.ToProto(), Hash: hash[:]}
		if height >= payoutHeight {
			begin.LastCommitInfo.Votes = votes
		}
		started := app.BeginBlock(begin)
		end := abci.RequestEndBlock{Height: height}
		ended := app.EndBlock(end)
		commit := app.Commit()
		previous = commit.Data
		if height < payoutHeight {
			continue
		}
		events, err := tmjson.Marshal(app.GetEventsDB().LoadEvents(uint32(height)))
		must(err)
		trace = append(trace, block{
			height, record(abci.ToRequestBeginBlock(begin), abci.ToResponseBeginBlock(started)),
			record(abci.ToRequestEndBlock(end), abci.ToResponseEndBlock(ended)),
			record(abci.ToRequestCommit(), abci.ToResponseCommit(commit)),
			hex.EncodeToString(previous), app.GetEmission().String(), events,
		})
	}
	must(app.Close())
	checkpoint := filepath.Join(dir, "checkpoint")
	manifest, err := os.ReadFile(filepath.Join(checkpoint, "manifest.json"))
	must(err)
	snapshot, err := os.ReadFile(filepath.Join(checkpoint, "snapshot.zlib"))
	must(err)
	fmt.Printf("%s: %s\n", s.name, trace[0].AppHash)
	return fixture{s.name, manifest, snapshot, trace}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: go run make_reward_payout_fixture.go NEW_OUTPUT_DIRECTORY")
	}
	must(os.Mkdir(os.Args[1], 0755))
	cases := []scenario{
		{name: "v3-before-v310", v310: -1, vote: true},
		{name: "v4-before-v320", v310: -30, v320: -1, vote: true},
		{name: "v5bug-before-v330", v310: -60, v320: -30, v330: -1, vote: true},
		{name: "v4-no-accrual", v310: -30},
		{name: "v5bug-no-accrual", v310: -60, v320: -30},
		{name: "v5fix-no-accrual", v310: -2000, v320: -1000, v330: -721, correction: true},
		// The activation heights for this case come from the mainnet checkpoint.
		{name: "v5fix-correction", v310: 10_217_000 - payoutHeight, v320: 10_509_400 - payoutHeight,
			v330: 10_523_500 - payoutHeight, vote: true, correction: true},
		{name: "v5fix-emission-cap", v310: -2000, v320: -1000, v330: -721, vote: true, cap: true},
	}
	fixtures := make([]fixture, 0, len(cases))
	for _, s := range cases {
		fixtures = append(fixtures, generate(filepath.Join(os.Args[1], s.name), s))
	}
	data, err := json.MarshalIndent(fixtures, "", "  ")
	must(err)
	must(os.WriteFile(filepath.Join(os.Args[1], "go-reward-payouts-v034.json"), append(data, '\n'), 0644))
}
