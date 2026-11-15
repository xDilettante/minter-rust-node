// Run from the pinned Minter Go module.
package main

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"os"

	events "github.com/MinterTeam/minter-go-node/coreV2/events"
	"github.com/MinterTeam/minter-go-node/coreV2/state/accounts"
	"github.com/MinterTeam/minter-go-node/coreV2/state/bus"
	"github.com/MinterTeam/minter-go-node/coreV2/state/checker"
	"github.com/MinterTeam/minter-go-node/coreV2/state/swap"
	"github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/tree"
	"github.com/cosmos/iavl"
	tmjson "github.com/tendermint/tendermint/libs/json"
	db "github.com/tendermint/tm-db"
)

type entry struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: go run make_order_fixture.go OUTPUT")
	}
	store, err := tree.NewMutableTree(0, db.NewMemDB(), 1024, 0)
	check(err)
	stateBus := bus.NewBus()
	checker.NewChecker(stateBus)
	eventStore := &events.MockEvents{}
	stateBus.SetEvents(eventStore)
	balances := accounts.NewAccounts(stateBus, store.GetLastImmutable())
	swapper := swap.NewV2(stateBus, store.GetLastImmutable())
	owner, second := types.Address{1}, types.Address{2}
	balances.AddBalance(owner, 3, big.NewInt(31))
	swapper.PairCreate(0, 2, big.NewInt(3_000_000), big.NewInt(2_000_000))
	swapper.PairCreate(0, 256, big.NewInt(5_000_000), big.NewInt(4_000_000))
	swapper.PairAddOrder(0, 2, big.NewInt(3712), big.NewInt(1684), owner, 1)
	swapper.PairAddOrder(2, 0, big.NewInt(2334), big.NewInt(1979), owner, 2)
	swapper.PairAddOrder(0, 256, big.NewInt(1059), big.NewInt(2010), second, 2)
	swapper.PairAddOrder(0, 2, big.NewInt(1963), big.NewInt(2490), second, 3)
	var seed []entry
	iavl.MutationHook = func(_ *iavl.MutableTree, key, value []byte) {
		seed = append(seed, entry{hex.EncodeToString(key), hex.EncodeToString(value)})
	}
	before, height, err := store.Commit(balances, swapper)
	check(err)
	if height != 1 {
		panic("unexpected initial order version")
	}
	iavl.MutationHook = nil

	swapper.ExpireOrders(2)
	after, height, err := store.Commit(balances, swapper)
	check(err)
	if height != 2 || len(eventStore.LoadEvents(0)) != 3 {
		panic("unexpected Go order expiration")
	}
	eventJSON, err := tmjson.Marshal(eventStore.LoadEvents(0))
	check(err)
	result, err := json.MarshalIndent(map[string]any{
		"before": hex.EncodeToString(before), "after": hex.EncodeToString(after),
		"seed": seed, "expireThrough": 2, "refundCount": len(eventStore.LoadEvents(0)),
		"events": json.RawMessage(eventJSON),
	}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[1], append(result, '\n'), 0644))
	fmt.Printf("Go expired three orders, AppHash %x\n", after)
}
