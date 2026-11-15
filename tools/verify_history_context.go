// Run from the pinned Minter Go module to independently check compact history.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"runtime"
	"time"

	"github.com/gogo/protobuf/proto"
	"github.com/tendermint/tendermint/libs/protoio"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	st "github.com/tendermint/tendermint/proto/tendermint/state"
	pb "github.com/tendermint/tendermint/proto/tendermint/types"
	tm "github.com/tendermint/tendermint/types"
)

type packageHeader struct {
	Chain    string      `protobuf:"bytes,1,opt,name=chain,proto3"`
	Anchor   uint64      `protobuf:"varint,2,opt,name=anchor,proto3"`
	From     uint64      `protobuf:"varint,3,opt,name=from,proto3"`
	AnchorID *pb.BlockID `protobuf:"bytes,4,opt,name=anchor_id,json=anchorId,proto3"`
}

func (m *packageHeader) Reset()         { *m = packageHeader{} }
func (m *packageHeader) String() string { return proto.CompactTextString(m) }
func (*packageHeader) ProtoMessage()    {}

type record struct {
	Header     []byte `protobuf:"bytes,1,opt,name=signed_header,json=signedHeader,proto3"`
	Validators []byte `protobuf:"bytes,2,opt,name=validators,proto3"`
	Evidence   []byte `protobuf:"bytes,3,opt,name=evidence,proto3"`
}

func (m *record) Reset()         { *m = record{} }
func (m *record) String() string { return proto.CompactTextString(m) }
func (*record) ProtoMessage()    {}

func check(err error) {
	if err != nil {
		panic(err)
	}
}
func require(ok bool, reason string) {
	if !ok {
		panic(reason)
	}
}

func main() {
	require(len(os.Args) == 3 || len(os.Args) == 4, "usage: verify_history_context CONTEXT TRUSTED_ANCHOR_BLOCKS [STATE]")
	anchorFile, err := os.Open(os.Args[2])
	check(err)
	defer anchorFile.Close()
	var envelope bc.Message
	_, err = protoio.NewDelimitedReader(anchorFile, 108*1024*1024).ReadMsg(&envelope)
	check(err)
	require(envelope.GetBlockResponse() != nil, "missing anchor BlockResponse")
	anchor, err := tm.BlockFromProto(envelope.GetBlockResponse().Block)
	check(err)
	anchorID := tm.BlockID{Hash: anchor.Hash(), PartSetHeader: anchor.MakePartSet(tm.BlockPartSizeBytes).Header()}
	file, err := os.Open(os.Args[1])
	check(err)
	defer file.Close()
	digest := sha256.New()
	input := io.TeeReader(file, digest)
	magic := make([]byte, len("MINTER-CONTEXT\x01"))
	_, err = io.ReadFull(input, magic)
	check(err)
	require(string(magic) == "MINTER-CONTEXT\x01", "wrong context version")
	reader := protoio.NewDelimitedReader(input, 108*1024*1024)
	var header packageHeader
	_, err = reader.ReadMsg(&header)
	check(err)
	id, err := tm.BlockIDFromProto(header.AnchorID)
	check(err)
	require(header.Chain == anchor.ChainID && header.Anchor == uint64(anchor.Height) && id.Equals(anchorID), "anchor differs")
	require(header.From > 0 && header.From < header.Anchor, "invalid declared range")
	successor := anchor.Header
	records, included, changes := 0, 0, 0
	var previousSet []byte
	var newerTime time.Time
	for h := header.Anchor - 1; ; h-- {
		var r record
		_, err = reader.ReadMsg(&r)
		check(err)
		var raw pb.SignedHeader
		check(raw.Unmarshal(r.Header))
		signed, err := tm.SignedHeaderFromProto(&raw)
		check(err)
		check(signed.ValidateBasic(header.Chain))
		var rawSet pb.ValidatorSet
		check(rawSet.Unmarshal(r.Validators))
		// This format carries membership, without proposer priorities. Go's
		// ValidatorSetFromProto requires a proposer used by light blocks.
		members := make([]*tm.Validator, len(rawSet.Validators))
		for i, raw := range rawSet.Validators {
			members[i], err = tm.ValidatorFromProto(raw)
			check(err)
		}
		set, err := tm.ValidatorSetFromExistingValidators(members)
		check(err)
		require(signed.Height == int64(h) && successor.Height == signed.Height+1 && signed.ChainID == successor.ChainID, "wrong height or chain")
		require(bytes.Equal(signed.Hash(), successor.LastBlockID.Hash) && signed.Commit.BlockID.Equals(successor.LastBlockID), "wrong historical link")
		require(bytes.Equal(signed.Commit.Hash(), successor.LastCommitHash), "wrong chain-selected commit")
		require(bytes.Equal(set.Hash(), signed.ValidatorsHash) && bytes.Equal(signed.NextValidatorsHash, successor.ValidatorsHash), "wrong validator membership")
		require(!signed.Time.After(successor.Time), "historical time is after successor")
		check(set.VerifyCommit(header.Chain, signed.Commit.BlockID, signed.Height, signed.Commit))
		var proofs pb.EvidenceList
		check(proofs.Unmarshal(r.Evidence))
		evidence := tm.EvidenceList{}
		seen := map[string]bool{}
		for _, rawProof := range proofs.Evidence {
			proof, err := tm.EvidenceFromProto(&rawProof)
			check(err)
			check(proof.ValidateBasic())
			hash := hex.EncodeToString(proof.Hash())
			require(!seen[hash], "repeated evidence")
			seen[hash] = true
			evidence = append(evidence, proof)
		}
		require(bytes.Equal(evidence.Hash(), signed.EvidenceHash), "wrong EvidenceHash")
		if !bytes.Equal(previousSet, set.Hash()) {
			changes++
			previousSet = set.Hash()
		}
		records++
		included += len(evidence)
		newerTime = successor.Time
		successor = *signed.Header
		if h == header.From {
			break
		}
	}
	var extra record
	n, err := reader.ReadMsg(&extra)
	require(n == 0 && err == io.EOF, "trailing context data")
	report := map[string]any{"platform": runtime.GOOS, "go": runtime.Version(), "anchor": header.Anchor, "from": header.From, "records": records, "includedProofs": included, "validatorSegments": changes, "sha256": fmt.Sprintf("%x", digest.Sum(nil))}
	if len(os.Args) == 4 {
		raw, err := os.ReadFile(os.Args[3])
		check(err)
		var state st.State
		check(state.Unmarshal(raw))
		require(state.LastBlockHeight == anchor.Height && state.LastBlockTime.Equal(anchor.Time), "state anchor differs")
		params := state.ConsensusParams.Evidence
		expired := func(height uint64, at time.Time) bool {
			return anchor.Height-int64(height) > params.MaxAgeNumBlocks && anchor.Time.Sub(at) > params.MaxAgeDuration
		}
		require(header.From == uint64(state.InitialHeight) || expired(header.From, successor.Time), "context does not cover both age limits")
		require(!expired(header.From+1, newerTime), "context exceeds the first expired boundary")
		report["maxAgeBlocks"] = params.MaxAgeNumBlocks
		report["maxAgeSeconds"] = params.MaxAgeDuration.Seconds()
		report["oldestTime"] = successor.Time
		report["anchorTime"] = anchor.Time
		report["ageSeconds"] = anchor.Time.Sub(successor.Time).Seconds()
		report["firstExpiredBoundaryVerified"] = true
	}
	check(json.NewEncoder(os.Stdout).Encode(report))
}
