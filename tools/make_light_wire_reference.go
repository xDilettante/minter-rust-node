// Run from the pinned Go oracle. Keys and headers are deterministic local fixtures.
package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"time"

	"github.com/tendermint/tendermint/crypto"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/crypto/merkle"
	"github.com/tendermint/tendermint/crypto/secp256k1"
	"github.com/tendermint/tendermint/evidence"
	bc "github.com/tendermint/tendermint/proto/tendermint/blockchain"
	pb "github.com/tendermint/tendermint/proto/tendermint/types"
	tm "github.com/tendermint/tendermint/types"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func hx(b []byte) string { return hex.EncodeToString(b) }

func field(tag uint64, data []byte) []byte {
	var prefix [20]byte
	n := binary.PutUvarint(prefix[:], tag<<3|2)
	n += binary.PutUvarint(prefix[n:], uint64(len(data)))
	return append(append([]byte(nil), prefix[:n]...), data...)
}

func withoutField(raw []byte, target uint64) []byte {
	var result []byte
	for len(raw) > 0 {
		key, n := binary.Uvarint(raw)
		if n <= 0 {
			panic("invalid block tag")
		}
		end := n
		switch key & 7 {
		case 0:
			_, size := binary.Uvarint(raw[n:])
			end += size
		case 1:
			end += 8
		case 2:
			size, m := binary.Uvarint(raw[n:])
			end += m + int(size)
		case 5:
			end += 4
		default:
			panic("unknown block wire type")
		}
		if key>>3 != target {
			result = append(result, raw[:end]...)
		}
		raw = raw[end:]
	}
	return result
}

var keys = map[string]crypto.PrivKey{}

const chain = "minter-light-wire-fixture"

func signed(header tm.Header, set *tm.ValidatorSet, round int32) *tm.SignedHeader {
	header.ValidatorsHash = set.Hash()
	id := tm.BlockID{Hash: header.Hash(), PartSetHeader: tm.PartSetHeader{Total: 1, Hash: make([]byte, 32)}}
	commit := &tm.Commit{Height: header.Height, Round: round, BlockID: id}
	for i, val := range set.Validators {
		vote := &tm.Vote{Type: pb.PrecommitType, Height: header.Height, Round: round,
			BlockID: id, Timestamp: header.Time, ValidatorAddress: val.Address, ValidatorIndex: int32(i)}
		var err error
		vote.Signature, err = keys[hx(val.PubKey.Bytes())].Sign(tm.VoteSignBytes(chain, vote.ToProto()))
		must(err)
		commit.Signatures = append(commit.Signatures, vote.CommitSig())
	}
	return &tm.SignedHeader{Header: &header, Commit: commit}
}

func proof(base tm.Header, common *tm.ValidatorSet, lunatic bool) *tm.LightClientAttackEvidence {
	h := base
	if lunatic {
		h.Height++
		h.AppHash = make([]byte, 32)
		h.AppHash[0] = 9
	} else {
		h.DataHash = make([]byte, 32)
		h.DataHash[0] = 7
	}
	trusted := signed(base, common, 0)
	result := &tm.LightClientAttackEvidence{ConflictingBlock: &tm.LightBlock{SignedHeader: signed(h, common, 0), ValidatorSet: common.Copy()},
		CommonHeight: base.Height, Timestamp: base.Time, TotalVotingPower: common.TotalVotingPower()}
	result.ByzantineValidators = result.GetByzantineValidators(common, trusted)
	return result
}

func resign(p *pb.LightClientAttackEvidence, set *tm.ValidatorSet) {
	h, err := tm.HeaderFromProto(p.ConflictingBlock.SignedHeader.Header)
	must(err)
	s := signed(h, set, p.ConflictingBlock.SignedHeader.Commit.Round)
	p.ConflictingBlock.SignedHeader = s.ToProto()
	p.ConflictingBlock.ValidatorSet, err = set.ToProto()
	must(err)
}

func validate(raw *pb.Evidence, common *tm.ValidatorSet, trusted *tm.SignedHeader) (accepted bool, reason string) {
	defer func() {
		if value := recover(); value != nil {
			accepted = false
			reason = fmt.Sprint("Go panic: ", value)
		}
	}()
	decoded, err := tm.EvidenceFromProto(raw)
	if err == nil {
		err = decoded.ValidateBasic()
	}
	if err == nil {
		err = evidence.VerifyLightClientAttack(decoded.(*tm.LightClientAttackEvidence), trusted, trusted, common, trusted.Time, time.Hour)
	}
	if err != nil {
		return false, err.Error()
	}
	return true, ""
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: make_light_wire_reference OUTPUT_JSON")
	}
	var vals []*tm.Validator
	for i := 0; i < 4; i++ {
		key := ed25519.GenPrivKeyFromSecret([]byte(fmt.Sprintf("public minter light wire key %d", i)))
		keys[hx(key.PubKey().Bytes())] = key
		vals = append(vals, tm.NewValidator(key.PubKey(), 1))
	}
	set := tm.NewValidatorSet(vals)
	h := tm.Header{ChainID: chain, Height: 101, Time: time.Date(2025, 1, 1, 0, 0, 0, 0, time.UTC), ValidatorsHash: set.Hash(), NextValidatorsHash: set.Hash(), ConsensusHash: make([]byte, 32), AppHash: make([]byte, 32), ProposerAddress: set.Validators[0].Address}
	h.Version.Block = 11
	h.Version.App = 300
	trusted := signed(h, set, 0)
	commonPB, err := set.ToProto()
	must(err)
	commonBytes, err := commonPB.Marshal()
	must(err)
	trustedBytes, err := trusted.ToProto().Marshal()
	must(err)
	type variant struct {
		name    string
		lunatic bool
		change  func(*pb.LightClientAttackEvidence)
	}
	variants := []variant{
		{"equivocation", false, nil}, {"lunatic", true, nil},
		{"multi_part_data", false, nil},
		{"nil_invalid_signature", false, func(p *pb.LightClientAttackEvidence) {
			s := &p.ConflictingBlock.SignedHeader.Commit.Signatures[3]
			s.BlockIdFlag = pb.BlockIDFlagNil
			s.Signature = make([]byte, 64)
		}},
		{"nil_short_signature", false, func(p *pb.LightClientAttackEvidence) {
			s := &p.ConflictingBlock.SignedHeader.Commit.Signatures[3]
			s.BlockIdFlag = pb.BlockIDFlagNil
			s.Signature = []byte{1}
		}},
		{"trailing_short_commit", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[3].Signature = []byte{1}
		}},
		{"early_invalid_commit", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[0].Signature = make([]byte, 64)
		}},
		{"absent_address", false, func(p *pb.LightClientAttackEvidence) {
			s := &p.ConflictingBlock.SignedHeader.Commit.Signatures[3]
			s.BlockIdFlag = pb.BlockIDFlagAbsent
			s.Signature = nil
			s.Timestamp = time.Time{}
			p.ByzantineValidators = p.ByzantineValidators[:3]
		}},
		{"byzantine_missing_key", true, func(p *pb.LightClientAttackEvidence) { p.ByzantineValidators[0].PubKey.Reset() }},
		{"byzantine_other_key", true, func(p *pb.LightClientAttackEvidence) {
			p.ByzantineValidators[0].PubKey = p.ByzantineValidators[1].PubKey
		}},
		{"proposer_other_address", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.ValidatorSet.Proposer.Address = make([]byte, 20)
		}},
		{"unsorted_validators", true, func(p *pb.LightClientAttackEvidence) {
			s := set.Copy()
			s.Validators[0], s.Validators[1] = s.Validators[1], s.Validators[0]
			resign(p, s)
		}},
		{"different_chain_id", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Header.ChainID = "another-local-chain"
			resign(p, set)
		}},
		{"empty_chain_id", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Header.ChainID = ""
			resign(p, set)
		}},
		{"partial_part_set", false, func(p *pb.LightClientAttackEvidence) {
			c := p.ConflictingBlock.SignedHeader.Commit
			c.BlockID.PartSetHeader = pb.PartSetHeader{}
			for i, val := range set.Validators {
				v := &tm.Vote{Type: pb.PrecommitType, Height: c.Height, Round: c.Round, BlockID: tm.BlockID{Hash: c.BlockID.Hash}, Timestamp: c.Signatures[i].Timestamp, ValidatorAddress: val.Address, ValidatorIndex: int32(i)}
				c.Signatures[i].Signature, err = keys[hx(val.PubKey.Bytes())].Sign(tm.VoteSignBytes(chain, v.ToProto()))
				must(err)
			}
		}},
		{"wire_amnesia", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Round = 1
			p.ByzantineValidators = nil
			resign(p, set)
		}},
		{"cached_total_ignored", true, func(p *pb.LightClientAttackEvidence) { p.ConflictingBlock.ValidatorSet.TotalVotingPower = 99999 }},
		{"zero_power_tail", true, func(p *pb.LightClientAttackEvidence) {
			s := set.Copy()
			key := ed25519.GenPrivKeyFromSecret([]byte("public zero power tail"))
			keys[hx(key.PubKey().Bytes())] = key
			s.Validators = append(s.Validators, tm.NewValidator(key.PubKey(), 0))
			resign(p, s)
			p.ConflictingBlock.SignedHeader.Commit.Signatures[4] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlagAbsent}
		}},
		{"secp256k1", true, func(p *pb.LightClientAttackEvidence) {
			s := set.Copy()
			key := secp256k1.GenPrivKeySecp256k1([]byte("public minter light secp key"))
			keys[hx(key.PubKey().Bytes())] = key
			s.Validators = append([]*tm.Validator{tm.NewValidator(key.PubKey(), 100)}, s.Validators...)
			resign(p, s)
		}},
		{"signature_address_is_not_index", false, func(p *pb.LightClientAttackEvidence) {
			c := p.ConflictingBlock.SignedHeader.Commit
			c.Signatures[0].ValidatorAddress = append([]byte(nil), set.Validators[3].Address...)
			c.Signatures[3] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlagAbsent}
			p.ByzantineValidators = p.ByzantineValidators[1:]
		}},
		{"validator_other_address", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.ValidatorSet.Validators[3].Address = make([]byte, 20)
		}},
		{"missing_proposer", true, func(p *pb.LightClientAttackEvidence) { p.ConflictingBlock.ValidatorSet.Proposer = nil }},
		{"absent_timestamp", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[3] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlagAbsent, Timestamp: time.Unix(0, 0)}
		}},
		{"absent_signature", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[3] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlagAbsent, Signature: []byte{1}}
		}},
		{"unknown_signature_flag", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[3] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlagUnknown}
		}},
		{"zero_common_height", true, func(p *pb.LightClientAttackEvidence) { p.CommonHeight = 0 }},
		{"zero_total_power", true, func(p *pb.LightClientAttackEvidence) { p.TotalVotingPower = 0 }},
		{"short_proposer", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Header.ProposerAddress = []byte{1}
		}},
		{"arbitrary_app_hash", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Header.AppHash = []byte{1}
			resign(p, set)
		}},
		{"empty_next_validators_hash", true, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Header.NextValidatorsHash = nil
			resign(p, set)
		}},
		{"wrapped_commit_flag", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[0].BlockIdFlag = pb.BlockIDFlag(258)
		}},
		{"negative_wrapped_commit_flag", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[0].BlockIdFlag = pb.BlockIDFlag(-254)
		}},
		{"wrapped_absent_flag", false, func(p *pb.LightClientAttackEvidence) {
			p.ConflictingBlock.SignedHeader.Commit.Signatures[3] = pb.CommitSig{BlockIdFlag: pb.BlockIDFlag(257)}
			p.ByzantineValidators = p.ByzantineValidators[:3]
		}},
		{"byzantine_secp_key", true, func(p *pb.LightClientAttackEvidence) {
			key := secp256k1.GenPrivKeySecp256k1([]byte("public byzantine secp key"))
			val, err := tm.NewValidator(key.PubKey(), 1).ToProto()
			must(err)
			p.ByzantineValidators[0].PubKey = val.PubKey
		}},
		{"missing_outer_data", true, nil},
		{"missing_outer_last_commit", true, nil},
		{"wrong_outer_commit_hash", true, nil},
		{"missing_proof_timestamp", true, nil},
		{"missing_outer_last_id", true, nil},
	}
	var rows []map[string]any
	for _, v := range variants {
		original := proof(h, set, v.lunatic)
		raw, err := tm.EvidenceToProto(original)
		must(err)
		p := raw.GetLightClientAttackEvidence()
		if v.change != nil {
			v.change(p)
		}
		wire, err := raw.Marshal()
		must(err)
		if v.name == "missing_proof_timestamp" {
			payload, err := p.Marshal()
			must(err)
			wire = field(2, withoutField(payload, 5))
		}
		var roundTrip pb.Evidence
		must(roundTrip.Unmarshal(wire))
		raw = &roundTrip
		p = raw.GetLightClientAttackEvidence()
		accepted, reason := validate(raw, set, trusted)
		var txs tm.Txs
		if v.name == "multi_part_data" {
			txs = tm.Txs{bytes.Repeat([]byte{0x21}, int(tm.BlockPartSizeBytes)+17)}
		}
		body := tm.MakeBlock(200, txs, trusted.Commit, nil)
		body.Header = h
		body.Header.Height = 200
		body.Header.DataHash = body.Data.Hash()
		body.Header.LastCommitHash = append([]byte(nil), trusted.Commit.Hash()...)
		outer, err := body.ToProto()
		must(err)
		outer.Evidence = pb.EvidenceList{Evidence: []pb.Evidence{*raw}}
		payload, err := p.Marshal()
		must(err)
		decodedEvidence, basicError := tm.EvidenceFromProto(raw)
		if basicError == nil {
			payload = decodedEvidence.Bytes()
		}
		outer.Header.EvidenceHash = merkle.HashFromByteSlices([][]byte{payload})
		if v.name == "wrong_outer_commit_hash" {
			outer.Header.LastCommitHash[0] ^= 1
		}
		decodedBlock, decodeError := tm.BlockFromProto(outer)
		blockWire, err := (&bc.Message{Sum: &bc.Message_BlockResponse{BlockResponse: &bc.BlockResponse{Block: outer}}}).Marshal()
		must(err)
		if v.name == "missing_outer_data" || v.name == "missing_outer_last_commit" {
			rawBlock, err := outer.Marshal()
			must(err)
			target := uint64(2)
			if v.name == "missing_outer_last_commit" {
				target = 4
			}
			blockWire = field(3, field(1, withoutField(rawBlock, target)))
			var msg bc.Message
			must(msg.Unmarshal(blockWire))
			decodedBlock, decodeError = tm.BlockFromProto(msg.GetBlockResponse().Block)
		}
		if v.name == "missing_outer_last_id" {
			header, err := outer.Header.Marshal()
			must(err)
			body, err := outer.Marshal()
			must(err)
			body = append(field(1, withoutField(header, 5)), withoutField(body, 1)...)
			blockWire = field(3, field(1, body))
			var msg bc.Message
			must(msg.Unmarshal(blockWire))
			decodedBlock, decodeError = tm.BlockFromProto(msg.GetBlockResponse().Block)
		}
		if accepted && decodeError != nil && v.name != "missing_outer_last_commit" && v.name != "wrong_outer_commit_hash" {
			panic(fmt.Sprintf("invalid fixture block %s: %v", v.name, decodeError))
		}
		row := map[string]any{"name": v.name, "proof": hx(wire), "accepted": accepted, "reason": reason, "block": hx(blockWire), "blockDecoded": decodeError == nil, "evidenceSize": outer.Evidence.Size()}
		if basicError == nil {
			row["evidenceHash"] = hx(decodedEvidence.Hash())
			abciRows := []string{}
			for _, entry := range decodedEvidence.ABCI() {
				raw, err := entry.Marshal()
				must(err)
				abciRows = append(abciRows, hx(raw))
			}
			row["abciEvidence"] = abciRows
		}
		if decodeError == nil {
			header, err := decodedBlock.Header.ToProto().Marshal()
			must(err)
			row["canonicalHeader"] = hx(header)
			parts := decodedBlock.MakePartSet(tm.BlockPartSizeBytes)
			canonical, err := decodedBlock.ToProto()
			must(err)
			row["blockHash"] = hx(decodedBlock.Hash())
			row["partSetHash"] = hx(parts.Header().Hash)
			row["partCount"] = parts.Total()
			row["canonicalSize"] = canonical.Size()
		}
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool { return rows[i]["name"].(string) < rows[j]["name"].(string) })
	result := map[string]any{"oracle": "Tendermint 0.34.19", "chainId": chain, "commonValidators": hx(commonBytes), "trustedHeader": hx(trustedBytes), "cases": rows}
	encoded, err := json.MarshalIndent(result, "", "  ")
	must(err)
	must(os.WriteFile(os.Args[1], append(encoded, '\n'), 0644))
}
