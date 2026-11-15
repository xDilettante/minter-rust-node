// Local, publicly seeded fixture wallets only. No production keys are accepted.
package main

import (
	"crypto/ecdsa"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"strconv"

	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/crypto"
	"github.com/MinterTeam/minter-go-node/helpers"
	"github.com/MinterTeam/minter-go-node/rlp"
	"github.com/tendermint/tendermint/crypto/ed25519"
	tmjson "github.com/tendermint/tendermint/libs/json"
	tm "github.com/tendermint/tendermint/types"
)

func check(err error) {
	if err != nil {
		panic(err)
	}
}
func pub(role string) mt.Pubkey {
	var result mt.Pubkey
	copy(result[:], ed25519.GenPrivKeyFromSecret([]byte("public Minter evidence fixture "+role)).PubKey().Bytes())
	return result
}

func main() {
	if len(os.Args) < 5 {
		panic("usage: make_candidate_key_transactions GENESIS NONCE rotate OLD_ROLE NEW_ROLE | GENESIS NONCE lock DUE_HEIGHT")
	}
	doc, err := tm.GenesisDocFromFile(os.Args[1])
	check(err)
	if doc.ChainID != "minter-rust-key-context-fixture" {
		panic("expected the local key-context fixture")
	}
	var genesis mt.AppState
	check(tmjson.Unmarshal(doc.AppState, &genesis))
	if len(genesis.Candidates) != 2 {
		panic("expected the two-candidate local fixture")
	}
	owner := genesis.Candidates[0].OwnerAddress
	var signer *ecdsa.PrivateKey
	for _, label := range []string{"sender", "receiver", "staker", "asset creator"} {
		key, err := crypto.ToECDSA(crypto.Keccak256([]byte("public Minter fixture " + label)))
		check(err)
		if crypto.PubkeyToAddress(key.PublicKey) == owner {
			signer = key
			break
		}
	}
	if signer == nil {
		panic("owner is not one of the public fixture wallets")
	}
	nonce, err := strconv.ParseUint(os.Args[2], 10, 64)
	check(err)
	var data transaction.Data
	switch os.Args[3] {
	case "rotate":
		if len(os.Args) != 6 {
			panic("rotate requires old/new fixture roles")
		}
		data = transaction.EditCandidatePublicKeyData{PubKey: pub(os.Args[4]), NewPubKey: pub(os.Args[5])}
	case "lock":
		if len(os.Args) != 5 {
			panic("lock requires a due height")
		}
		due, err := strconv.ParseUint(os.Args[4], 10, 32)
		check(err)
		data = transaction.LockData{DueBlock: uint32(due), Coin: 0, Value: helpers.BipToPip(big.NewInt(1))}
	default:
		panic("unknown fixture transaction")
	}
	raw, err := rlp.EncodeToBytes(data)
	check(err)
	tx := transaction.Transaction{Nonce: nonce, ChainID: mt.ChainMainnet, GasPrice: 1, GasCoin: 0,
		Type: data.TxType(), Data: raw, SignatureType: transaction.SigTypeSingle}
	check(tx.Sign(signer))
	raw, err = tx.Serialize()
	check(err)
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{"tx": hex.EncodeToString(raw), "owner": owner.String(), "nonce": nonce,
		"tendermintHash": fmt.Sprintf("%x", tm.Tx(raw).Hash()), "type": fmt.Sprintf("%02x", uint8(data.TxType()))}))
}
