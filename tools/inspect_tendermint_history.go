// Build from the pinned Tendermint v0.34.19 module.
package main

import (
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strconv"

	"github.com/syndtr/goleveldb/leveldb/opt"
	sm "github.com/tendermint/tendermint/state"
	blockkv "github.com/tendermint/tendermint/state/indexer/block/kv"
	txkv "github.com/tendermint/tendermint/state/txindex/kv"
	"github.com/tendermint/tendermint/store"
	dbm "github.com/tendermint/tm-db"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func open(name, dir string) *dbm.GoLevelDB {
	db, err := dbm.NewGoLevelDBWithOpts(name, dir, &opt.Options{ReadOnly: true, ErrorIfMissing: true})
	must(err)
	return db
}

func count(db dbm.DB, prefix string) int {
	var iter dbm.Iterator
	var err error
	if prefix == "" {
		iter, err = db.Iterator(nil, nil)
	} else {
		iter, err = dbm.IteratePrefix(db, []byte(prefix))
	}
	must(err)
	defer iter.Close()
	n := 0
	for ; iter.Valid(); iter.Next() {
		n++
	}
	must(iter.Error())
	return n
}

func bytesOnDisk(root string) int64 {
	var total int64
	must(filepath.Walk(root, func(_ string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if info.Mode().IsRegular() {
			total += info.Size()
		}
		return nil
	}))
	return total
}

func main() {
	if len(os.Args) != 4 {
		panic("usage: inspect_tendermint_history HOME HEIGHT TX_HASH_HEX")
	}
	dir := filepath.Join(os.Args[1], "data")
	height, err := strconv.ParseInt(os.Args[2], 10, 64)
	must(err)
	hash, err := hex.DecodeString(os.Args[3])
	must(err)
	blockDB := open("blockstore", dir)
	defer blockDB.Close()
	stateDB := open("state", dir)
	defer stateDB.Close()
	indexDB := open("tx_index", dir)
	defer indexDB.Close()
	blocks := store.NewBlockStore(blockDB)
	state := sm.NewStore(stateDB)
	tx, err := txkv.NewTxIndex(indexDB).Get(hash)
	must(err)
	blocksIndexed, err := blockkv.New(dbm.NewPrefixDB(indexDB, []byte("block_events"))).Has(height)
	must(err)
	_, resultsError := state.LoadABCIResponses(height)
	_, baseResultsError := state.LoadABCIResponses(blocks.Base())
	_, baseValidatorsError := state.LoadValidators(blocks.Base())
	floor, err := indexDB.Get([]byte("\x00minter/tendermint-retention/index-floor/v1"))
	must(err)
	var indexFloor uint64
	if len(floor) != 0 {
		if len(floor) != 8 {
			panic("invalid index history floor")
		}
		indexFloor = binary.BigEndian.Uint64(floor)
	}
	must(json.NewEncoder(os.Stdout).Encode(map[string]interface{}{
		"base": blocks.Base(), "head": blocks.Height(), "indexFloor": indexFloor,
		"blockAvailable": blocks.LoadBlock(height) != nil, "resultsAvailable": resultsError == nil,
		"baseBlockAvailable":   blocks.LoadBlock(blocks.Base()) != nil,
		"baseResultsAvailable": baseResultsError == nil, "baseValidatorsAvailable": baseValidatorsError == nil,
		"txAvailable": tx != nil, "blockIndexed": blocksIndexed,
		"blockMetadataKeys": count(blockDB, "H:"), "blockPartKeys": count(blockDB, "P:"),
		"abciResultKeys": count(stateDB, "abciResponsesKey:"),
		"txHeightKeys":   count(indexDB, fmt.Sprintf("tx.height/%d/", height)),
		"indexKeys":      count(indexDB, ""),
		"blockBytes":     bytesOnDisk(filepath.Join(dir, "blockstore.db")),
		"stateBytes":     bytesOnDisk(filepath.Join(dir, "state.db")),
		"indexBytes":     bytesOnDisk(filepath.Join(dir, "tx_index.db")),
	}))
}
