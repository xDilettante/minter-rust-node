// Run from the pinned Minter Go module.
package main

import (
	"os"

	pb "github.com/MinterTeam/node-grpc-gateway/api_pb"
	statuspb "google.golang.org/genproto/googleapis/rpc/status"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protodesc"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/types/descriptorpb"
)

func main() {
	if len(os.Args) != 2 {
		panic("usage: export_api_v2_schema OUTPUT.pb")
	}
	schema := &descriptorpb.FileDescriptorSet{}
	seen := make(map[string]bool)
	var add func(protoreflect.FileDescriptor)
	add = func(file protoreflect.FileDescriptor) {
		if seen[file.Path()] {
			return
		}
		seen[file.Path()] = true
		imports := file.Imports()
		for i := 0; i < imports.Len(); i++ {
			add(imports.Get(i).FileDescriptor)
		}
		schema.File = append(schema.File, protodesc.ToFileDescriptorProto(file))
	}
	for _, file := range []protoreflect.FileDescriptor{
		pb.File_api_proto, pb.File_data_proto, pb.File_events_proto, pb.File_swagger_proto,
		statuspb.File_google_rpc_status_proto,
	} {
		add(file)
	}
	data, err := proto.MarshalOptions{Deterministic: true}.Marshal(schema)
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], data, 0644); err != nil {
		panic(err)
	}
}
