// Run with go run -mod=readonly from the pinned Minter Go module.
package main

import (
	"encoding/json"
	"math/big"
	"os"

	"github.com/MinterTeam/minter-go-node/formula"
)

type sample struct {
	Supply  string `json:"supply"`
	Reserve string `json:"reserve"`
	Ratio   uint32 `json:"ratio"`
	Sold    string `json:"sold"`
}

func amount(text string) *big.Int {
	n, ok := new(big.Int).SetString(text, 10)
	if !ok {
		panic("invalid amount")
	}
	return n
}

func main() {
	samples := []sample{
		{"4976500020267004859854967", "5861669219341420066476566", 50, "719024844304529406944142"},
		{"27469540187614366600761778", "133925450674578924409823786", 75, "75149791183841912458887"},
		{"207446542368969603621698188", "6394798873997673269859534", 70, "13187968207946834966100092"},
		{"3695547482344096037060218", "4625667595527557939594806", 63, "232185556576164002395707"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 33, "0"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 33, "1"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 100, "1"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 33, "500000000000000000000000000000000"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 33, "999999999999999999999999999999999"},
		{"1000000000000000000000000000000000", "999999999999999999999999999999999", 33, "1000000000000000000000000000000000"},
		{"1267650600228229401496703205377", "1000000000000000000000000001", 25, "633825300114114700748351602689"},
		{"1267650600228229401496703205377", "1000000000000000000000000001", 100, "633825300114114700748351602689"},
	}
	type result struct {
		sample
		Return         string `json:"return"`
		WantReceive    string `json:"wantReceive"`
		SaleAmount     string `json:"saleAmount"`
		PurchaseReturn string `json:"purchaseReturn"`
		PurchaseAmount string `json:"purchaseAmount"`
	}
	values := make([]result, 0, len(samples))
	for _, s := range samples {
		supply, reserve, sold := amount(s.Supply), amount(s.Reserve), amount(s.Sold)
		got := formula.CalculateSaleReturn(supply, reserve, s.Ratio, sold)
		wanted := new(big.Int).Div(new(big.Int).Mul(sold, reserve), supply)
		needed := formula.CalculateSaleAmount(supply, reserve, s.Ratio, wanted)
		values = append(values, result{
			sample: s, Return: got.String(), WantReceive: wanted.String(), SaleAmount: needed.String(),
			PurchaseReturn: formula.CalculatePurchaseReturn(supply, reserve, s.Ratio, wanted).String(),
			PurchaseAmount: formula.CalculatePurchaseAmount(supply, reserve, s.Ratio, sold).String(),
		})
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(values); err != nil {
		panic(err)
	}
}
