//go:build ignore

package main

import (
	"bytes"
	"fmt"
	"os/exec"
	"strings"
)

func main() {
	fmt.Println("Testing Rate Limiting on CheckUsername...")
	fmt.Println("------------------------------------------")

	target := "127.0.0.1:50051"
	service := "chat.v1.AuthService/CheckUsername"
	
	for i := 0; i < 20; i++ {
		fmt.Printf(".")
		cmd := exec.Command("grpcurl", "-plaintext", "-d", `{"username": "test-user-rate-limit"}`, target, service)
		var stderr bytes.Buffer
		cmd.Stderr = &stderr
		err := cmd.Run()
		
		if err != nil {
			if strings.Contains(stderr.String(), "RateLimited") || strings.Contains(stderr.String(), "ResourceExhausted") {
				fmt.Printf("\nSUCCESS: Rate limiting kicked in after %d requests!\n", i+1)
				return
			} else {
				fmt.Printf("\nERROR after %d requests: %s\n", i+1, stderr.String())
				return
			}
		}
	}
	fmt.Println("\nNO RATE LIMIT detected after 20 fast requests.")
}
