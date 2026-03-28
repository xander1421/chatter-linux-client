//go:build ignore

package main

import (
	"bytes"
	"fmt"
	"os/exec"
	"strings"
)

func main() {
	fmt.Println("Starting Manual Security Tests for Chatter Backend...")
	fmt.Println("-----------------------------------------------------")

	tests := []struct {
		Name     string
		Service  string
		Method   string
		Input    string
		Expected string
	}{
		{
			"Login with non-existing user",
			"chat.v1.AuthService",
			"Login",
			`{"username": "non-existing-user-12345", "password_hash": "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=", "svr_public_key": "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=", "device_info": "Linux", "request_id": "test"}`,
			"Unauthenticated",
		},
		{
			"GetPreKeyBundle without auth",
			"chat.v1.AuthService",
			"GetPreKeyBundle",
			`{"user_id": "test-user"}`,
			"Unauthenticated",
		},
		{
			"GetMailboxTokens without auth",
			"chat.v1.mailbox.EphemeralMailboxService",
			"GetMailboxTokens",
			`{"count": 1}`,
			"Unauthenticated",
		},
		{
			"CreateEphemeralMailbox with random key/signature",
			"chat.v1.mailbox.EphemeralMailboxService",
			"CreateEphemeralMailbox",
			`{"public_key": "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=", "signature": "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE="}`,
			"invalid signature", // This service uses an error field in the response
		},
		{
			"ListChannels without auth",
			"chat.v1.ChannelService",
			"ListChannels",
			`{}`,
			"Unauthenticated",
		},
		{
			"GetUserProfile without auth",
			"chat.v1.UserService",
			"GetUserProfile",
			`{"user_id": "some-id"}`,
			"Unauthenticated",
		},
	}

	for _, t := range tests {
		fmt.Printf("Testing [%s]... ", t.Name)
		cmd := exec.Command("grpcurl", "-plaintext", "-d", t.Input, "127.0.0.1:50051", t.Service+"/"+t.Method)
		var stdout, stderr bytes.Buffer
		cmd.Stdout = &stdout
		cmd.Stderr = &stderr
		err := cmd.Run()

		actualErr := stderr.String()
		actualOut := stdout.String()
		
		if err != nil && (strings.Contains(actualErr, t.Expected) || strings.Contains(actualErr, "PermissionDenied")) {
			fmt.Println("PASSED")
		} else if err == nil && strings.Contains(actualOut, t.Expected) {
			fmt.Println("PASSED (Response: %s)", actualOut)
		} else if err == nil {
			fmt.Printf("UNEXPECTED SUCCESS (Output: %s)\n", actualOut)
		} else {
			// Some might fail with InvalidArgument if my input is still slightly wrong, 
			// but we want to know if it rejects it.
			if strings.Contains(actualErr, "InvalidArgument") {
				fmt.Printf("INPUT INVALID (Actual stderr: %s)\n", actualErr)
			} else {
				fmt.Printf("FAILED (Actual stderr: %s, Error: %v)\n", actualErr, err)
			}
		}
	}
}
