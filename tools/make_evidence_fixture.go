// Run from the pinned Go Minter module. All signing keys are public test keys.
package main

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	mt "github.com/MinterTeam/minter-go-node/coreV2/types"
	"github.com/MinterTeam/minter-go-node/helpers"
	abci "github.com/tendermint/tendermint/abci/types"
	"github.com/tendermint/tendermint/crypto/ed25519"
	"github.com/tendermint/tendermint/evidence"
	tmjson "github.com/tendermint/tendermint/libs/json"
	"github.com/tendermint/tendermint/libs/protoio"
	"github.com/tendermint/tendermint/p2p"
	"github.com/tendermint/tendermint/p2p/conn"
	"github.com/tendermint/tendermint/privval"
	consensuspb "github.com/tendermint/tendermint/proto/tendermint/consensus"
	p2ppb "github.com/tendermint/tendermint/proto/tendermint/p2p"
	tmproto "github.com/tendermint/tendermint/proto/tendermint/types"
	core "github.com/tendermint/tendermint/rpc/core/types"
	tm "github.com/tendermint/tendermint/types"
	"github.com/tendermint/tendermint/version"
)

var chain = "minter-rust-evidence-fixture"
var fixtureInitialHeight int64

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func key(role string) ed25519.PrivKey {
	return ed25519.GenPrivKeyFromSecret([]byte("public Minter evidence fixture " + role))
}

func pubkey(role string) mt.Pubkey {
	var result mt.Pubkey
	copy(result[:], key(role).PubKey().Bytes())
	return result
}

func genesis(base, out, mode string) {
	doc, err := tm.GenesisDocFromFile(filepath.Join(base, "genesis.json"))
	check(err)
	var state mt.AppState
	check(tmjson.Unmarshal(doc.AppState, &state))
	if len(state.Candidates) != 1 || len(state.Candidates[0].Stakes) != 1 || len(state.Validators) != 1 {
		panic("expected the single-validator Minter ABCI fixture")
	}
	candidate := &state.Candidates[0]
	validator := &state.Validators[0]
	majority := helpers.BipToPip(big.NewInt(4000))
	if mode == "quorum" {
		majority = helpers.BipToPip(big.NewInt(3000))
	}
	minority := helpers.BipToPip(big.NewInt(1000))
	if mode == "consensus" {
		minority = helpers.BipToPip(big.NewInt(3000))
	}
	roles := []string{"minority"}
	if mode == "multinode" || mode == "quorum" {
		minority = new(big.Int).Set(majority)
		roles = append(roles, "third", "fourth")
	}
	if mode == "quorum" {
		roles = append(roles, "fifth", "sixth", "seventh")
	}
	previous, ok := new(big.Int).SetString(candidate.Stakes[0].Value, 10)
	if !ok || previous.Cmp(majority) >= 0 {
		panic("unexpected fixture stake")
	}
	cost := new(big.Int).Sub(majority, previous)
	cost.Add(cost, new(big.Int).Mul(minority, big.NewInt(int64(len(roles)))))
	var owner mt.Address
	funded := false
	for i := range state.Accounts {
		for j := range state.Accounts[i].Balance {
			balance := &state.Accounts[i].Balance[j]
			amount, ok := new(big.Int).SetString(balance.Value, 10)
			if balance.Coin == 0 && ok && amount.Cmp(cost) >= 0 {
				balance.Value = amount.Sub(amount, cost).String()
				owner = state.Accounts[i].Address
				funded = true
				break
			}
		}
		if funded {
			break
		}
	}
	if !funded {
		panic("fixture lacks BIP for the new stake")
	}
	candidate.PubKey = pubkey("majority")
	candidate.TotalBipStake = majority.String()
	candidate.Stakes[0].Value = candidate.TotalBipStake
	candidate.Stakes[0].BipValue = candidate.TotalBipStake
	validator.PubKey = candidate.PubKey
	validator.TotalBipStake = candidate.TotalBipStake
	if mode == "key-context" {
		candidate.OwnerAddress, candidate.ControlAddress = owner, owner
		doc.ConsensusParams.Version.AppVersion = 8
		state.Emission = helpers.BipToPip(big.NewInt(10_000_000_000)).String()
		state.Commission.EditCandidatePublicKey = "5"
		state.Commission.Lock = "2"
	}
	for i, role := range roles {
		minor := pubkey(role)
		state.Candidates = append(state.Candidates, mt.Candidate{
			ID: candidate.ID + uint64(i) + 1, PubKey: minor, Status: 2, Commission: 10,
			RewardAddress: owner, OwnerAddress: owner, ControlAddress: owner, TotalBipStake: minority.String(),
			Stakes: []mt.Stake{{Owner: owner, Coin: 0, Value: minority.String(), BipValue: minority.String()}},
		})
		state.Validators = append(state.Validators, mt.Validator{
			PubKey: minor, TotalBipStake: minority.String(), AccumReward: "0", AbsentTimes: mt.NewBitArray(24),
		})
	}
	check(state.Verify())
	doc.ChainID = chain
	if fixtureInitialHeight > 0 {
		doc.InitialHeight = fixtureInitialHeight
	}
	if mode == "consensus" {
		doc.ChainID = "minter-rust-consensus-fixture"
	}
	doc.Validators = nil
	doc.AppState, err = tmjson.Marshal(state)
	check(err)
	check(doc.ValidateAndComplete())
	check(os.Mkdir(out, 0755))
	check(doc.SaveAs(filepath.Join(out, "genesis.json")))
	privval.NewFilePV(key("majority"), filepath.Join(out, "priv_validator_key.json"),
		filepath.Join(out, "priv_validator_state.json")).Save()
	if mode == "consensus" || mode == "light-genesis" || mode == "multinode" || mode == "quorum" || mode == "key-context" {
		privval.NewFilePV(key("minority"), filepath.Join(out, "second_validator_key.json"),
			filepath.Join(out, "second_validator_state.json")).Save()
	}
	if mode == "key-context" {
		privval.NewFilePV(key("majority replacement"), filepath.Join(out, "replacement_validator_key.json"),
			filepath.Join(out, "replacement_validator_state.json")).Save()
		public := make(map[string]map[string]string)
		for _, role := range []string{"majority", "majority replacement", "minority", "minority replacement", "minority replacement 2"} {
			public[role] = map[string]string{"address": fmt.Sprintf("%X", key(role).PubKey().Address()), "publicKey": pubkey(role).String()}
		}
		raw, err := json.MarshalIndent(public, "", "  ")
		check(err)
		check(os.WriteFile(filepath.Join(out, "fixture-public.json"), append(raw, '\n'), 0644))
	}
	if mode == "multinode" || mode == "quorum" {
		for _, role := range roles[1:] {
			privval.NewFilePV(key(role), filepath.Join(out, role+"_validator_key.json"),
				filepath.Join(out, role+"_validator_state.json")).Save()
		}
	}
	if mode == "quorum" {
		peers := make(map[string]string)
		for _, role := range append([]string{"majority"}, roles...) {
			prefix := role + "_validator"
			if role == "majority" {
				prefix = "priv_validator"
			} else if role == "minority" {
				prefix = "second_validator"
			}
			peer := &p2p.NodeKey{PrivKey: key("peer " + role)}
			check(peer.SaveAs(filepath.Join(out, prefix+"_node_key.json")))
			peers[prefix] = string(peer.ID())
		}
		raw, err := json.MarshalIndent(peers, "", "  ")
		check(err)
		check(os.WriteFile(filepath.Join(out, "quorum-peers.json"), append(raw, '\n'), 0644))
	}
}

func proposers(path, countText string) {
	raw, err := os.ReadFile(path)
	check(err)
	var result core.ResultValidators
	check(tmjson.Unmarshal(raw, &result))
	var metadata struct {
		Proposer string `json:"proposer"`
	}
	check(json.Unmarshal(raw, &metadata))
	count, err := strconv.Atoi(countText)
	check(err)
	if count < 1 || count > 64 || len(result.Validators) == 0 {
		panic("expected a nonempty validator set and 1..64 rounds")
	}
	set := &tm.ValidatorSet{Validators: result.Validators}
	for _, validator := range set.Validators {
		if strings.EqualFold(fmt.Sprintf("%X", validator.Address), metadata.Proposer) {
			set.Proposer = validator.Copy()
		}
	}
	if set.Proposer == nil {
		panic("missing initial proposer from Go consensus state")
	}
	check(set.ValidateBasic())
	sequence := make([]string, 0, count)
	for i := 0; i < count; i++ {
		sequence = append(sequence, fmt.Sprintf("%X", set.GetProposer().Address))
		set.IncrementProposerPriority(1)
	}
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{
		"height": result.BlockHeight, "votingPower": set.TotalVotingPower(), "proposers": sequence,
	}))
}

func duplicateVote(blockPath, validatorsPath string) {
	var block core.ResultBlock
	raw, err := os.ReadFile(blockPath)
	check(err)
	check(tmjson.Unmarshal(raw, &block))
	var validators core.ResultValidators
	raw, err = os.ReadFile(validatorsPath)
	check(err)
	check(tmjson.Unmarshal(raw, &validators))
	if block.Block == nil || block.Block.ChainID != chain || block.Block.Height != validators.BlockHeight {
		panic("expected a block and validator set from the local evidence fixture")
	}
	minor := key("minority")
	set := &tm.ValidatorSet{Validators: validators.Validators}
	index, validator := set.GetByAddress(minor.PubKey().Address())
	if validator == nil {
		panic("minority test key is not in the committed validator set")
	}
	alternate := block.BlockID
	alternate.Hash = append([]byte(nil), block.BlockID.Hash...)
	alternate.Hash[0] ^= 1
	sign := func(id tm.BlockID) *tm.Vote {
		vote := &tm.Vote{
			Type: tmproto.PrecommitType, Height: block.Block.Height, Round: 0,
			BlockID: id, Timestamp: block.Block.Time, ValidatorAddress: validator.Address,
			ValidatorIndex: index,
		}
		vote.Signature, err = minor.Sign(tm.VoteSignBytes(chain, vote.ToProto()))
		check(err)
		return vote
	}
	proof := tm.NewDuplicateVoteEvidence(sign(block.BlockID), sign(alternate), block.Block.Time, set)
	check(proof.ValidateBasic())
	check(evidence.VerifyDuplicateVote(proof, chain, set))
	payload, err := tmjson.Marshal(proof)
	check(err)
	nilProof := tm.NewDuplicateVoteEvidence(sign(block.BlockID), sign(tm.BlockID{}), block.Block.Time, set)
	check(nilProof.ValidateBasic())
	check(evidence.VerifyDuplicateVote(nilProof, chain, set))
	nilPayload, err := tmjson.Marshal(nilProof)
	check(err)
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{
		"evidence": json.RawMessage(payload), "hash": fmt.Sprintf("%X", proof.Hash()),
		"nilEvidence": json.RawMessage(nilPayload), "nilHash": fmt.Sprintf("%X", nilProof.Hash()),
		"validator": pubkey("minority").String(),
	}))
}

func lightAttack(blockPath, validatorsPath string, shortNil bool) {
	var block core.ResultBlock
	raw, err := os.ReadFile(blockPath)
	check(err)
	check(tmjson.Unmarshal(raw, &block))
	var validators core.ResultValidators
	raw, err = os.ReadFile(validatorsPath)
	check(err)
	check(tmjson.Unmarshal(raw, &validators))
	if block.Block == nil || block.Block.ChainID != chain || block.Block.Height != validators.BlockHeight {
		panic("expected a signed header from the local evidence fixture")
	}
	majority := key("majority")
	set := &tm.ValidatorSet{Validators: validators.Validators}
	index, signer := set.GetByAddress(majority.PubKey().Address())
	if signer == nil || len(set.Validators) != 2 || signer.VotingPower*3 <= set.TotalVotingPower()*2 {
		panic("expected the two fixture validators")
	}
	set.Proposer = signer
	conflict := block.Block.Header
	conflict.Height++
	conflict.AppHash = append([]byte(nil), conflict.ValidatorsHash...)
	id := tm.BlockID{Hash: conflict.Hash(), PartSetHeader: block.BlockID.PartSetHeader}
	vote := &tm.Vote{Type: tmproto.PrecommitType, Height: conflict.Height,
		Round: 0, BlockID: id, Timestamp: conflict.Time, ValidatorAddress: signer.Address, ValidatorIndex: index}
	vote.Signature, err = majority.Sign(tm.VoteSignBytes(chain, vote.ToProto()))
	check(err)
	signatures := []tm.CommitSig{tm.NewCommitSigAbsent(), tm.NewCommitSigAbsent()}
	signatures[index] = vote.CommitSig()
	if shortNil {
		other := 1 - int(index)
		signatures[other] = tm.CommitSig{BlockIDFlag: tm.BlockIDFlagNil, ValidatorAddress: set.Validators[other].Address,
			Timestamp: conflict.Time, Signature: []byte{1}}
	}
	proof := &tm.LightClientAttackEvidence{
		ConflictingBlock: &tm.LightBlock{
			SignedHeader: &tm.SignedHeader{Header: &conflict,
				Commit: &tm.Commit{Height: conflict.Height, Round: vote.Round, BlockID: id, Signatures: signatures}},
			ValidatorSet: set,
		},
		CommonHeight: block.Block.Height, TotalVotingPower: set.TotalVotingPower(), Timestamp: block.Block.Time,
		ByzantineValidators: []*tm.Validator{signer},
	}
	check(proof.ValidateBasic())
	trusted := &tm.SignedHeader{Header: &block.Block.Header}
	check(evidence.VerifyLightClientAttack(proof, trusted, trusted, set, trusted.Time, time.Hour))
	payload, err := tmjson.Marshal(proof)
	check(err)
	check(json.NewEncoder(os.Stdout).Encode(map[string]any{
		"evidence": json.RawMessage(payload), "hash": fmt.Sprintf("%X", proof.Hash()),
	}))
}

// Send a local fixture proof over the Tendermint wire. The caller picks a
// signed fixture, including negative vectors. No production chain is used.
func relay(mode, peer, path string) {
	endpoint, err := p2p.NewNetAddressString(peer)
	check(err)
	if !endpoint.IP.IsLoopback() {
		panic("the evidence fixture only connects to loopback")
	}
	var raw json.RawMessage
	file, err := os.Open(path)
	check(err)
	defer file.Close()
	check(json.NewDecoder(file).Decode(&raw))
	peerKey := ed25519.GenPrivKey()
	socket, err := net.DialTimeout("tcp", endpoint.DialString(), 5*time.Second)
	check(err)
	defer socket.Close()
	check(socket.SetDeadline(time.Now().Add(8 * time.Second)))
	secret, err := conn.MakeSecretConnection(socket, peerKey)
	check(err)
	if p2p.PubKeyToID(secret.RemotePubKey()) != endpoint.ID {
		panic("unexpected Rust peer")
	}
	node := p2p.DefaultNodeInfo{
		ProtocolVersion: p2p.NewProtocolVersion(version.P2PProtocol, version.BlockProtocol, 0),
		DefaultNodeID:   p2p.PubKeyToID(peerKey.PubKey()), ListenAddr: "127.0.0.1:16656",
		Network: chain, Version: "0.34.19", Moniker: "Minter evidence fixture",
		Channels: []byte{0x20, 0x21, 0x22, 0x23, 0x38, 0x40},
	}
	writer := protoio.NewDelimitedWriter(secret)
	reader := protoio.NewDelimitedReader(secret, 10*1024)
	_, err = writer.WriteMsg(node.ToProto())
	check(err)
	var remote p2ppb.DefaultNodeInfo
	_, err = reader.ReadMsg(&remote)
	check(err)
	if remote.Network != chain {
		panic("expected the local Minter evidence fixture")
	}
	send := func(channel int32, data []byte) {
		for len(data) > 0 {
			size := len(data)
			if size > 1024 {
				size = 1024
			}
			_, err := writer.WriteMsg(&p2ppb.Packet{Sum: &p2ppb.Packet_PacketMsg{
				PacketMsg: &p2ppb.PacketMsg{ChannelID: channel, EOF: size == len(data), Data: data[:size]},
			}})
			check(err)
			data = data[size:]
		}
	}
	switch mode {
	case "relay":
		var proof tm.Evidence
		check(tmjson.Unmarshal(raw, &proof))
		proto, err := tm.EvidenceToProto(proof)
		check(err)
		data, err := (&tmproto.EvidenceList{Evidence: []tmproto.Evidence{*proto}}).Marshal()
		check(err)
		send(0x38, data)
	case "relay-votes":
		var heightText string
		check(json.Unmarshal(raw, &heightText))
		height, err := strconv.ParseInt(heightText, 10, 64)
		check(err)
		minor := key("minority")
		// The unmodified fixture gives the majority signer index zero.
		partHash := make([]byte, 32)
		partHash[0] = 3
		for _, id := range []tm.BlockID{
			{},
			{Hash: partHash, PartSetHeader: tm.PartSetHeader{Total: 1, Hash: partHash}},
		} {
			vote := &tm.Vote{Type: tmproto.PrevoteType, Height: height, Round: 0,
				BlockID: id, Timestamp: time.Now().UTC(), ValidatorAddress: minor.PubKey().Address(), ValidatorIndex: 1}
			vote.Signature, err = minor.Sign(tm.VoteSignBytes(chain, vote.ToProto()))
			check(err)
			data, err := (&consensuspb.Message{Sum: &consensuspb.Message_Vote{
				Vote: &consensuspb.Vote{Vote: vote.ToProto()},
			}}).Marshal()
			check(err)
			send(0x22, data)
		}
	}
	time.Sleep(100 * time.Millisecond)
}

func main() {
	for len(os.Args) >= 3 && strings.HasPrefix(os.Args[1], "--") {
		switch os.Args[1] {
		case "--chain-id":
			chain = os.Args[2]
		case "--initial-height":
			var err error
			fixtureInitialHeight, err = strconv.ParseInt(os.Args[2], 10, 64)
			check(err)
			if fixtureInitialHeight < 1 {
				panic("initial height must be positive")
			}
		default:
			panic("unknown fixture option")
		}
		os.Args = append(os.Args[:1], os.Args[3:]...)
	}
	if len(os.Args) == 3 && os.Args[1] == "end-block" {
		var result core.ResultBlockResults
		raw, err := os.ReadFile(os.Args[2])
		check(err)
		check(tmjson.Unmarshal(raw, &result))
		end := abci.ResponseEndBlock{ValidatorUpdates: result.ValidatorUpdates,
			ConsensusParamUpdates: result.ConsensusParamUpdates, Events: result.EndBlockEvents}
		encoded, err := end.Marshal()
		check(err)
		check(json.NewEncoder(os.Stdout).Encode(map[string]any{
			"height": result.Height, "validatorUpdates": len(end.ValidatorUpdates),
			"endResponseSha256": fmt.Sprintf("%x", sha256.Sum256(encoded)),
		}))
		return
	}
	if len(os.Args) != 4 {
		panic("usage: make_evidence_fixture genesis|consensus|multinode|quorum BASE_FIXTURE OUTPUT | proposers VALIDATORS_JSON COUNT | vote BLOCK_JSON VALIDATORS_JSON | light COMMIT_JSON VALIDATORS_JSON | relay|relay-votes PEER PROOF_JSON | end-block BLOCK_RESULTS_JSON")
	}
	switch os.Args[1] {
	case "genesis", "consensus", "light-genesis", "multinode", "quorum", "key-context":
		genesis(os.Args[2], os.Args[3], os.Args[1])
	case "proposers":
		proposers(os.Args[2], os.Args[3])
	case "vote":
		duplicateVote(os.Args[2], os.Args[3])
	case "light", "light-short-nil":
		lightAttack(os.Args[2], os.Args[3], os.Args[1] == "light-short-nil")
	case "relay", "relay-votes":
		relay(os.Args[1], os.Args[2], os.Args[3])
	default:
		panic("expected genesis, consensus or vote")
	}
}
