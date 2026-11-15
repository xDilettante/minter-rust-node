// Run from the pinned Minter Go module with go run -mod=readonly.
package main

import (
	"bufio"
	"compress/gzip"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/MinterTeam/minter-go-node/coreV2/transaction"
	"github.com/MinterTeam/minter-go-node/rlp"
	abci "github.com/tendermint/tendermint/abci/types"
)

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 3 {
		panic("usage: go run export_minter_transaction_oracle.go ABCI_TRACE OUTPUT")
	}
	input, err := os.Open(os.Args[1])
	check(err)
	defer input.Close()
	source, err := gzip.NewReader(input)
	check(err)
	defer source.Close()
	decoder := json.NewDecoder(source)
	file, err := os.Create(os.Args[2])
	check(err)
	buffer := bufio.NewWriter(file)
	output := json.NewEncoder(buffer)
	blocks, transactions := 0, 0
	counts := map[transaction.SigType]int{}
	for {
		var block struct {
			Height    uint64 `json:"height"`
			DeliverTx []struct {
				Request  []byte `json:"request"`
				Response []byte `json:"response"`
			} `json:"deliver_tx"`
		}
		err := decoder.Decode(&block)
		if errors.Is(err, io.EOF) {
			break
		}
		check(err)
		blocks++
		for index, delivery := range block.DeliverTx {
			var request abci.RequestDeliverTx
			var response abci.ResponseDeliverTx
			check(request.Unmarshal(delivery.Request))
			check(response.Unmarshal(delivery.Response))
			record := map[string]any{
				"height": block.Height, "index": index, "raw": hex.EncodeToString(request.Tx),
				"deliverCode": response.Code,
			}
			var tx transaction.Transaction
			if err := rlp.DecodeBytes(request.Tx, &tx); err != nil {
				record["decodeError"] = err.Error()
			} else {
				record["nonce"] = tx.Nonce
				record["chainId"] = tx.ChainID
				record["gasPrice"] = tx.GasPrice
				record["gasCoin"] = tx.GasCoin
				record["type"] = tx.Type
				record["data"] = hex.EncodeToString(tx.Data)
				record["payload"] = hex.EncodeToString(tx.Payload)
				record["serviceData"] = hex.EncodeToString(tx.ServiceData)
				record["signatureType"] = tx.SignatureType
				record["signatureData"] = hex.EncodeToString(tx.SignatureData)
				hash := tx.Hash()
				record["signingHash"] = hex.EncodeToString(hash[:])
				counts[tx.SignatureType]++
				if _, err := transaction.DecodeSig(&tx); err != nil {
					record["signatureError"] = err.Error()
				} else if sender, err := tx.Sender(); err != nil {
					record["senderError"] = err.Error()
				} else {
					record["sender"] = hex.EncodeToString(sender[:])
					if tx.SignatureType == transaction.SigTypeMulti {
						var multi transaction.SignatureMulti
						check(rlp.DecodeBytes(tx.SignatureData, &multi))
						signers := make([]*string, 0, len(multi.Signatures))
						for _, signature := range multi.Signatures {
							account, err := transaction.RecoverPlain(hash, signature.R, signature.S, signature.V)
							if err != nil {
								signers = append(signers, nil)
							} else {
								encoded := hex.EncodeToString(account[:])
								signers = append(signers, &encoded)
							}
						}
						record["signers"] = signers
					}
				}
			}
			check(output.Encode(record))
			transactions++
		}
	}
	check(buffer.Flush())
	check(file.Close())
	fmt.Printf("Decoded %d transactions from %d blocks, signature types %v\n", transactions, blocks, counts)
}
