// Run with go run from the pinned Minter Go module.
package main

import (
	"bytes"
	"compress/gzip"
	"compress/zlib"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"

	"github.com/MinterTeam/minter-go-node/coreV2/appdb/types"
	"github.com/MinterTeam/minter-go-node/coreV2/state/accounts"
	"github.com/MinterTeam/minter-go-node/coreV2/state/bus"
	mintertypes "github.com/MinterTeam/minter-go-node/coreV2/types"
	ics23 "github.com/confio/ics23/go"
	"github.com/cosmos/iavl"
	protoio "github.com/gogo/protobuf/io"
	db "github.com/tendermint/tm-db"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

type valueAtKey struct {
	Key   string  `json:"key"`
	Value *string `json:"value"`
	Proof string  `json:"proof,omitempty"`
}

func main() {
	trace := flag.String("trace", "", "optional gzip Go IAVL trace to apply")
	through := flag.Int64("through", 0, "last height to apply from the trace")
	balanceReads := flag.Bool("balances", false, "read balances by ADDRESS_HEX:COIN_ID")
	proofs := flag.Bool("proofs", false, "include Go ICS23 proofs for each raw key")
	flag.Parse()
	args := flag.Args()
	if len(args) < 3 {
		panic("usage: go run read_go_checkpoint.go [-trace FILE -through HEIGHT] [-balances|-proofs] SNAPSHOT HEIGHT KEY_HEX|ADDRESS_HEX:COIN_ID...")
	}
	if *balanceReads && *proofs {
		panic("-balances and -proofs are mutually exclusive")
	}
	input, err := os.Open(args[0])
	must(err)
	defer input.Close()
	height, err := strconv.ParseInt(args[1], 10, 64)
	must(err)
	z, err := zlib.NewReader(input)
	must(err)
	defer z.Close()

	tree, err := iavl.NewMutableTree(db.NewMemDB(), 0)
	must(err)
	importer, err := tree.Import(height)
	must(err)
	defer importer.Close()
	reader := protoio.NewDelimitedReader(z, 64_000_000)
	seenState := false
	for {
		item := new(types.SnapshotItem)
		err := reader.ReadMsg(item)
		if errors.Is(err, io.EOF) {
			break
		}
		must(err)
		if store := item.GetStore(); store != nil {
			seenState = seenState || store.Name == "state"
			continue
		}
		node := item.GetIAVL()
		if !seenState || node == nil || node.Height < 0 || node.Height > 127 {
			panic("invalid IAVL snapshot item")
		}
		key, value := node.Key, node.Value
		if key == nil {
			key = []byte{}
		}
		if node.Height == 0 && value == nil {
			value = []byte{}
		}
		must(importer.Add(&iavl.ExportNode{
			Key: key, Value: value, Version: node.Version, Height: int8(node.Height),
		}))
	}
	if !seenState {
		panic("missing state store")
	}
	must(importer.Commit())

	switch {
	case *trace != "" && *through > height:
		replay(tree, *trace, height, *through)
		height = *through
	case *trace != "" || *through != 0:
		panic("-trace and -through require a target above the snapshot height")
	}

	result := map[string]any{"height": height, "appHash": hex.EncodeToString(tree.Hash())}
	immutable, err := tree.GetImmutable(height)
	must(err)
	if *balanceReads {
		reader := accounts.NewAccounts(bus.NewBus(), immutable)
		result["balances"] = balances(reader, args[2:])
	} else {
		values := make([]valueAtKey, 0, len(args)-2)
		for _, encoded := range args[2:] {
			key, err := hex.DecodeString(encoded)
			must(err)
			_, value := immutable.Get(key)
			item := valueAtKey{Key: hex.EncodeToString(key)}
			if value != nil {
				raw := hex.EncodeToString(value)
				item.Value = &raw
			}
			if *proofs {
				item.Proof = prove(immutable, key, value != nil)
			}
			values = append(values, item)
		}
		result["values"] = values
	}
	var output bytes.Buffer
	writer := json.NewEncoder(&output)
	writer.SetIndent("", "  ")
	must(writer.Encode(result))
	fmt.Print(output.String())
}

func prove(tree *iavl.ImmutableTree, key []byte, present bool) string {
	var proof *ics23.CommitmentProof
	var err error
	if present {
		proof, err = tree.GetMembershipProof(key)
	} else {
		proof, err = tree.GetNonMembershipProof(key)
	}
	must(err)
	raw, err := proof.Marshal()
	must(err)
	return hex.EncodeToString(raw)
}

func balances(reader *accounts.Accounts, queries []string) []map[string]any {
	values := make([]map[string]any, 0, len(queries))
	for _, query := range queries {
		addressText, coinText, found := strings.Cut(query, ":")
		decoded, err := hex.DecodeString(addressText)
		must(err)
		if !found || len(decoded) != mintertypes.AddressLength {
			panic("balance query requires a 20-byte address and a coin ID")
		}
		coin, err := strconv.ParseUint(coinText, 10, 32)
		must(err)
		var address mintertypes.Address
		copy(address[:], decoded)
		values = append(values, map[string]any{
			"address": hex.EncodeToString(address[:]),
			"coin":    coin,
			"balance": reader.GetBalance(address, mintertypes.CoinID(coin)).String(),
			"nonce":   reader.GetNonce(address),
		})
	}
	return values
}

func replay(tree *iavl.MutableTree, trace string, start, through int64) {
	input, err := os.Open(trace)
	must(err)
	defer input.Close()
	gz, err := gzip.NewReader(input)
	must(err)
	defer gz.Close()
	decoder := json.NewDecoder(gz)
	for height := start + 1; height <= through; height++ {
		var block struct {
			Height  int64  `json:"height"`
			AppHash string `json:"app_hash"`
			Ops     []struct {
				Key    string `json:"key_hex"`
				Value  string `json:"value_hex"`
				Remove bool   `json:"remove"`
			} `json:"iavl_operations"`
		}
		must(decoder.Decode(&block))
		if block.Height != height {
			panic(fmt.Sprintf("expected height %d, got %d", height, block.Height))
		}
		for _, op := range block.Ops {
			key, err := hex.DecodeString(op.Key)
			must(err)
			if op.Remove {
				tree.Remove(key)
			} else {
				value, err := hex.DecodeString(op.Value)
				must(err)
				tree.Set(key, value)
			}
		}
		hash, version, err := tree.SaveVersion()
		must(err)
		if version != height || hex.EncodeToString(hash) != block.AppHash {
			panic(fmt.Sprintf("Go IAVL differs from trace at height %d", height))
		}
		if height-120 >= start {
			must(tree.DeleteVersion(height - 120))
		}
	}
}
