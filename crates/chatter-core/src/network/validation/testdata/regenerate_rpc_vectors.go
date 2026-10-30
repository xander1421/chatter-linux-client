//go:build ignore

// Re-evaluate synthetic wire fixtures with the server's official validator.
// Run from the server repository root, write to a temporary file, then replace
// rpc_validation_vectors.tsv after inspecting any changed verdicts:
// GOEXPERIMENT=runtimesecret go run linux-client/crates/chatter-core/src/network/validation/testdata/regenerate_rpc_vectors.go < linux-client/crates/chatter-core/src/network/validation/testdata/rpc_validation_vectors.tsv
package main

import (
	"buf.build/go/protovalidate"
	"bufio"
	_ "chatter/pkg/proto/chat/v1/auth"
	_ "chatter/pkg/proto/chat/v1/channel"
	_ "chatter/pkg/proto/chat/v1/integration"
	_ "chatter/pkg/proto/chat/v1/message"
	"encoding/hex"
	"fmt"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/reflect/protoregistry"
	"google.golang.org/protobuf/types/dynamicpb"
	"os"
	"strings"
)

func main() {
	v, e := protovalidate.New()
	if e != nil {
		panic(e)
	}
	s := bufio.NewScanner(os.Stdin)
	s.Buffer(make([]byte, 4096), 2000000)
	for s.Scan() {
		if strings.HasPrefix(s.Text(), "#") {
			fmt.Println(s.Text())
			continue
		}
		p := strings.Split(s.Text(), "\t")
		d, e := protoregistry.GlobalFiles.FindDescriptorByName(protoreflect.FullName(p[0]))
		if e != nil {
			panic(e)
		}
		m := dynamicpb.NewMessage(d.(protoreflect.MessageDescriptor))
		b, e := hex.DecodeString(p[3])
		if e != nil {
			panic(e)
		}
		if e = proto.Unmarshal(b, m); e != nil {
			panic(e)
		}
		e = v.Validate(m)
		fmt.Printf("%s\t%s\t%t\t%s\n", p[0], p[1], e == nil, p[3])
	}
	if s.Err() != nil {
		panic(s.Err())
	}
}
