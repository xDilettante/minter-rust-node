// Run with go run from the pinned Minter Go module. See the Rust repo README.
package main

import (
	"bytes"
	"compress/zlib"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"

	"github.com/MinterTeam/minter-go-node/coreV2/appdb/types"
	"github.com/cosmos/iavl"
	protoio "github.com/gogo/protobuf/io"
	db "github.com/tendermint/tm-db"
)

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: go run make_checkpoint_fixture.go OUT")
	}
	dir := os.Args[1]
	tree, err := iavl.NewMutableTree(db.NewMemDB(), 0)
	check(err)
	tree.Set([]byte("a"), []byte("original"))
	tree.Set([]byte("c"), []byte("removed"))
	tree.Set([]byte("m"), []byte{})
	tree.Set([]byte("z"), []byte("last"))
	_, _, err = tree.SaveVersion()
	check(err)
	tree.Set([]byte("a"), []byte("current"))
	tree.Remove([]byte("c"))
	tree.Set([]byte{0}, []byte{0, 255})
	root, height, err := tree.SaveVersion()
	check(err)

	var output bytes.Buffer
	z, err := zlib.NewWriterLevel(&output, 7)
	check(err)
	writer := protoio.NewDelimitedWriter(z)
	store := func(name string, value []byte) {
		check(writer.WriteMsg(&types.SnapshotItem{Item: &types.SnapshotItem_Store{
			Store: &types.SnapshotStoreItem{Name: name, Value: value},
		}}))
	}
	var rawHeight [8]byte
	binary.BigEndian.PutUint64(rawHeight[:], uint64(height))
	store("height", rawHeight[:])
	store("hash", root)
	store("versions", []byte{0xc0})
	store("state", nil)

	export := tree.Export()
	defer export.Close()
	nodes := 0
	for {
		node, err := export.Next()
		if err == iavl.ExportDone {
			break
		}
		check(err)
		check(writer.WriteMsg(&types.SnapshotItem{Item: &types.SnapshotItem_IAVL{
			IAVL: &types.SnapshotIAVLItem{Key: node.Key, Value: node.Value, Height: int32(node.Height), Version: node.Version},
		}}))
		nodes++
	}
	check(writer.Close())

	check(os.MkdirAll(dir, 0755))
	check(os.WriteFile(filepath.Join(dir, "go-checkpoint.zlib"), output.Bytes(), 0644))
	manifest, err := json.MarshalIndent(map[string]any{
		"format": 1, "network": "minter-test-fixture", "height": height, "nodes": nodes,
		"appHash": hex.EncodeToString(root), "blockHash": fmt.Sprintf("%064x", 0),
		"snapshotSha256": fmt.Sprintf("%x", sha256.Sum256(output.Bytes())), "bytes": output.Len(),
	}, "", "  ")
	check(err)
	check(os.WriteFile(filepath.Join(dir, "go-checkpoint.json"), append(manifest, '\n'), 0644))
	check(writeMutations(dir, tree))
}

type mutation struct {
	Key    string `json:"key_hex"`
	Value  string `json:"value_hex,omitempty"`
	Remove bool   `json:"remove,omitempty"`
}

type block struct {
	Height int64      `json:"height"`
	Hash   string     `json:"app_hash"`
	Ops    []mutation `json:"iavl_operations"`
}

func set(key, value []byte) mutation {
	return mutation{Key: hex.EncodeToString(key), Value: hex.EncodeToString(value)}
}

func remove(key []byte) mutation {
	return mutation{Key: hex.EncodeToString(key), Remove: true}
}

func writeMutations(dir string, tree *iavl.MutableTree) error {
	f, err := os.Create(filepath.Join(dir, "go-mutations.jsonl"))
	if err != nil {
		return err
	}
	out := json.NewEncoder(f)
	save := func(ops ...mutation) {
		for _, op := range ops {
			key, err := hex.DecodeString(op.Key)
			check(err)
			if op.Remove {
				tree.Remove(key)
			} else {
				value, err := hex.DecodeString(op.Value)
				check(err)
				tree.Set(key, value)
			}
		}
		root, height, err := tree.SaveVersion()
		check(err)
		check(out.Encode(block{height, hex.EncodeToString(root), ops}))
	}

	save(set([]byte("a"), []byte("same-height")), set([]byte("a"), []byte("current")))
	save(remove([]byte("missing")))
	save(remove([]byte{0}), remove([]byte("a")), remove([]byte("m")), remove([]byte("z")))
	save()
	save(set([]byte{}, []byte{}))
	save(set([]byte("b"), []byte("middle")), set([]byte("d"), []byte("upper")),
		set([]byte("c"), []byte("inner")), set([]byte("a"), []byte("lower")))
	save(remove([]byte("a")), remove([]byte("b")), remove([]byte{}))

	rng := rand.New(rand.NewSource(23))
	for range 120 {
		ops := make([]mutation, rng.Intn(12)+1)
		for i := range ops {
			key := []byte{byte(rng.Intn(80))}
			if rng.Intn(3) == 0 {
				ops[i] = remove(key)
			} else {
				ops[i] = set(key, []byte{byte(rng.Intn(256))})
			}
		}
		save(ops...)
	}
	return f.Close()
}
