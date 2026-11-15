// Run with go run -mod=readonly from the pinned Minter Go module.
package main

import (
	"encoding/json"
	"math/big"
	"os"
	"time"

	"github.com/MinterTeam/minter-go-node/config"
	"github.com/MinterTeam/minter-go-node/coreV2/appdb"
)

type result struct {
	Series string `json:"series"`
	Time   uint64 `json:"time"`
	BIP    string `json:"bip"`
	USDT   string `json:"usdt"`
	Reward string `json:"reward"`
	Safe   string `json:"safe"`
	Off    bool   `json:"off"`
}

func integer(s string) *big.Int {
	n, ok := new(big.Int).SetString(s, 10)
	if !ok {
		panic("invalid reserve")
	}
	return n
}

func main() {
	home, err := os.MkdirTemp("", "minter-price-fixture-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(home)
	cfg := config.GetConfig(home)
	cfg.DBBackend = "memdb"
	const bip = "1000000000000000000000000000"
	reserves := [][2]string{
		{bip, "100000000000000000000000"},
		{bip, "91000000000000000000000"},
		{bip, "82809999999999999999999"},
		{bip, "82300000000000000000000"},
		{bip, "83000000000000000000000"},
		{bip, "83100000000000000000000"},
		{bip, "83200000000000000000000"},
		{"1000000000000000000000000000000009", "1000000000000000000000000000000001"},
		{"1000000000000000000000000000000000", "1"},
		{"1000000000000000000000000000000000", "1"},
		{"1", "16"},
	}
	var results []result
	for _, series := range []string{"fixed", "legacy"} {
		db := appdb.NewAppDB(home, cfg)
		update := db.UpdatePriceFix
		if series == "legacy" {
			update = db.UpdatePriceBug
		}
		for i, pair := range reserves {
			t := time.Date(2025, 1, i+1, 12, 0, 0, i, time.UTC)
			reward, safe := update(t, integer(pair[0]), integer(pair[1]))
			_, _, _, _, off := db.GetPrice()
			results = append(results, result{
				series, uint64(t.UnixNano()), pair[0], pair[1], reward.String(), safe.String(), off,
			})
		}
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(results); err != nil {
		panic(err)
	}
}
