// Run from the pinned Minter Go module. This verifies a public historical
// corpus, not execution or a trusted path from genesis to these block headers.
package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/libs/protoio"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	tm "github.com/tendermint/tendermint/types"
)

type historicalCase struct {
	Name   string
	Height int64
	Hash   string
}

var cases = []historicalCase{
	{"legal", 15190549, "b41e54a6e63210247fced2f6553723389526a3ea006746f391b6ab07bc1cf0a7"},
	{"interstake", 20220176, "766b42b8cb18efa9b50f0b60a7ee0b77a8ec2a3a293cb38b00a8ed6f6d5ff529"},
	{"bazercash", 23497951, "ad4549948ad553cd5fe4f0fe57957618c16740c8cb54b832830736ce09dd730d"},
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func require(ok bool, format string, args ...any) {
	if !ok {
		panic(fmt.Sprintf(format, args...))
	}
}

func readJSON(path string, out any) {
	raw, err := os.ReadFile(path)
	check(err)
	check(json.Unmarshal(raw, out))
}

func readBlocks(path string) map[int64]*tm.Block {
	file, err := os.Open(path)
	check(err)
	defer file.Close()
	reader := protoio.NewDelimitedReader(file, tm.MaxBlockSizeBytes+1024)
	blocks := make(map[int64]*tm.Block)
	for {
		var message bc.Message
		count, err := reader.ReadMsg(&message)
		if err == io.EOF {
			require(count == 0, "truncated trailing block message")
			break
		}
		check(err)
		response := message.GetBlockResponse()
		require(response != nil, "expected a BlockResponse")
		block, err := tm.BlockFromProto(response.Block)
		check(err)
		check(block.ValidateBasic())
		require(block.ChainID == "minter-mainnet-5", "unexpected chain at %d", block.Height)
		require(blocks[block.Height] == nil, "duplicate block %d", block.Height)
		blocks[block.Height] = block
	}
	return blocks
}

func blockID(block *tm.Block) tm.BlockID {
	return tm.BlockID{Hash: block.Hash(), PartSetHeader: block.MakePartSet(tm.BlockPartSizeBytes).Header()}
}

// Make a negative fixture whose hashes are internally consistent, so that the
// verifier must reach signature verification rather than structural checks.
func makeInvalidCommit(directory string, successor bool) {
	path := filepath.Join(directory, "recreation-blocks.pb")
	blocks := readBlocks(path)
	require(len(blocks) == len(cases)*4, "unexpected negative-fixture block count")
	height := cases[0].Height + 1
	if successor {
		height++
	}
	next := blocks[height]
	require(next != nil && next.LastCommit != nil, "missing negative-fixture commit")
	commit, err := tm.CommitFromProto(next.LastCommit.ToProto())
	check(err)
	changed := false
	for i, signature := range commit.Signatures {
		if signature.BlockIDFlag == tm.BlockIDFlagCommit {
			commit.Signatures[i].Signature = append([]byte(nil), signature.Signature...)
			commit.Signatures[i].Signature[0] ^= 1
			changed = true
			break
		}
	}
	require(changed, "no commit signature to change")
	next.LastCommit = commit
	next.LastCommitHash = commit.Hash()
	check(next.ValidateBasic())
	if following := blocks[height+1]; following != nil {
		following.LastBlockID = blockID(next)
		commit, err := tm.CommitFromProto(following.LastCommit.ToProto())
		check(err)
		commit.BlockID = following.LastBlockID
		following.LastCommit = commit
		following.LastCommitHash = commit.Hash()
		check(following.ValidateBasic())
	}
	file, err := os.Create(path)
	check(err)
	writer := protoio.NewDelimitedWriter(file)
	for _, item := range cases {
		for height := item.Height - 1; height <= item.Height+2; height++ {
			block, err := blocks[height].ToProto()
			check(err)
			_, err = writer.WriteMsg(&bc.Message{Sum: &bc.Message_BlockResponse{
				BlockResponse: &bc.BlockResponse{Block: block},
			}})
			check(err)
		}
	}
	check(file.Close())
	apiPath := filepath.Join(directory, "monster-legal-blocks.json")
	var api map[string]any
	readJSON(apiPath, &api)
	api["result"].([]any)[1].(map[string]any)["hash"] = hex.EncodeToString(blocks[cases[0].Height+1].Hash())
	raw, err := json.Marshal(api)
	check(err)
	check(os.WriteFile(apiPath, append(raw, '\n'), 0644))
}

func validatorSet(path string) *tm.ValidatorSet {
	var response struct {
		Validators []struct {
			PublicKey string `json:"public_key"`
			Power     string `json:"voting_power"`
		} `json:"validators"`
	}
	readJSON(path, &response)
	var validators []*tm.Validator
	for _, validator := range response.Validators {
		require(strings.HasPrefix(validator.PublicKey, "Mp"), "invalid public-key prefix")
		key, err := hex.DecodeString(validator.PublicKey[2:])
		check(err)
		require(len(key) == ed25519.PubKeySize, "invalid validator key length")
		power, err := strconv.ParseInt(validator.Power, 10, 64)
		check(err)
		require(power > 0, "invalid validator power")
		validators = append(validators, tm.NewValidator(ed25519.PubKey(key), power))
	}
	return tm.NewValidatorSet(validators)
}

func main() {
	if len(os.Args) == 3 && (os.Args[1] == "--make-invalid-commit" || os.Args[1] == "--make-invalid-successor-commit") {
		makeInvalidCommit(os.Args[2], os.Args[1] == "--make-invalid-successor-commit")
		return
	}
	require(len(os.Args) == 3, "usage: verify_recreation_history CORPUS NEW_OUTPUT_JSON")
	directory := os.Args[1]
	blocks := readBlocks(filepath.Join(directory, "recreation-blocks.pb"))
	require(len(blocks) == len(cases)*4, "unexpected block count: %d", len(blocks))
	var records []map[string]any
	for _, item := range cases {
		previous, block, next := blocks[item.Height-1], blocks[item.Height], blocks[item.Height+1]
		confirmation := blocks[item.Height+2]
		require(previous != nil && block != nil && next != nil && confirmation != nil,
			"missing %s block range", item.Name)
		require(blockID(previous).Equals(block.LastBlockID) && blockID(block).Equals(next.LastBlockID) &&
			blockID(next).Equals(confirmation.LastBlockID),
			"broken block link for %s", item.Name)
		set := validatorSet(filepath.Join(directory, "monster-"+item.Name+"-validators.json"))
		require(bytes.Equal(set.Hash(), block.ValidatorsHash), "validator hash differs for %s", item.Name)
		err := set.VerifyCommit(block.ChainID, blockID(block), block.Height, next.LastCommit)
		require(err == nil, "target commit verification at %d: %v", block.Height, err)
		successorSet := validatorSet(filepath.Join(directory, "monster-"+item.Name+"-successor-validators.json"))
		require(bytes.Equal(successorSet.Hash(), next.ValidatorsHash), "successor validator hash differs for %s", item.Name)
		err = successorSet.VerifyCommit(next.ChainID, blockID(next), next.Height, confirmation.LastCommit)
		require(err == nil, "successor commit verification at %d: %v", next.Height, err)
		require(len(block.Txs) == 1 && hex.EncodeToString(block.Txs[0].Hash()) == item.Hash,
			"unexpected transaction for %s", item.Name)
		tx, err := transaction.NewExecutor(transaction.GetDataV3).DecodeFromBytes(block.Txs[0])
		check(err)
		data, ok := tx.GetDecodedData().(*transaction.RecreateTokenData)
		require(ok && tx.ChainID == 1, "unexpected transaction kind/network for %s", item.Name)
		sender, err := tx.Sender()
		check(err)
		var api struct {
			Result []struct {
				Height       int64  `json:"height"`
				Hash         string `json:"hash"`
				Transactions []struct {
					Hash     string  `json:"hash"`
					Height   int64   `json:"height"`
					Index    *uint32 `json:"index"`
					From     string  `json:"from"`
					Nonce    uint64  `json:"nonce"`
					GasPrice uint32  `json:"gas_price"`
					GasCoin  *uint32 `json:"gas_coin"`
					TypeHex  string  `json:"type_hex"`
					Code     *uint32 `json:"code"`
					Gas      int64   `json:"gas"`
					Data     struct {
						Name          string `json:"name"`
						Symbol        string `json:"symbol"`
						InitialAmount string `json:"initial_amount"`
						MaxSupply     string `json:"max_supply"`
						Mintable      *bool  `json:"mintable"`
						Burnable      *bool  `json:"burnable"`
					} `json:"data"`
				} `json:"transactions"`
			} `json:"result"`
		}
		readJSON(filepath.Join(directory, "monster-"+item.Name+"-blocks.json"), &api)
		require(len(api.Result) == 2 && api.Result[0].Height == item.Height &&
			api.Result[1].Height == item.Height+1 && len(api.Result[0].Transactions) == 1,
			"unexpected API range for %s", item.Name)
		require(api.Result[0].Hash == hex.EncodeToString(block.Hash()) &&
			api.Result[1].Hash == hex.EncodeToString(next.Hash()), "API block hash differs for %s", item.Name)
		observed := api.Result[0].Transactions[0]
		require(observed.Index != nil && observed.GasCoin != nil && observed.Code != nil &&
			observed.Data.Mintable != nil && observed.Data.Burnable != nil,
			"API transaction metadata incomplete for %s", item.Name)
		require(observed.Hash == "Mt"+item.Hash && observed.From == sender.String() &&
			observed.Height == item.Height && *observed.Index == 0 && observed.Nonce == tx.Nonce &&
			observed.GasPrice == tx.GasPrice && uint64(*observed.GasCoin) == uint64(tx.GasCoin) &&
			observed.TypeHex == tx.Type.String() && observed.Gas == tx.Gas() && *observed.Code == 0,
			"API transaction differs for %s", item.Name)
		require(observed.Data.Name == data.Name && observed.Data.Symbol == data.Symbol.String() &&
			observed.Data.InitialAmount == data.InitialAmount.String() &&
			observed.Data.MaxSupply == data.MaxSupply.String() && *observed.Data.Mintable == data.Mintable &&
			*observed.Data.Burnable == data.Burnable, "API payload differs for %s", item.Name)
		records = append(records, map[string]any{
			"case": item.Name, "height": item.Height, "blockHash": hex.EncodeToString(block.Hash()),
			"nextBlockHash": hex.EncodeToString(next.Hash()), "validatorsHash": hex.EncodeToString(set.Hash()),
			"signedThrough": next.Height, "successorValidatorsHash": hex.EncodeToString(successorSet.Hash()),
			"validators": set.Size(), "preAppHash": hex.EncodeToString(block.AppHash),
			"postAppHash": hex.EncodeToString(next.AppHash), "transactionHash": item.Hash,
			"rawTransaction": hex.EncodeToString(block.Txs[0]), "sender": sender.String(),
			"kind": tx.Type, "chainId": tx.ChainID, "nonce": tx.Nonce, "gasCoin": tx.GasCoin,
			"gasPrice": tx.GasPrice, "gas": tx.Gas(), "name": data.Name, "symbol": data.Symbol.String(),
			"initialAmount": data.InitialAmount.String(), "maxSupply": data.MaxSupply.String(),
			"mintable": data.Mintable, "burnable": data.Burnable,
		})
	}
	output, err := os.OpenFile(os.Args[2], os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0644)
	check(err)
	encoder := json.NewEncoder(output)
	encoder.SetIndent("", "  ")
	check(encoder.Encode(map[string]any{
		"format": 1, "chain": "minter-mainnet-5", "blocks": len(blocks),
		"commits": len(cases) * 2, "executionVerified": false, "trustedChainPathVerified": false,
		"cases": records,
	}))
	check(output.Close())
	fmt.Printf("Verified %d blocks and %d commits; execution and trusted chain path remain unverified.\n", len(blocks), len(cases)*2)
}
