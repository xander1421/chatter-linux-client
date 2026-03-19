#!/bin/bash

# Ensure we're in the project root
cd "$(dirname "$0")"

echo "Building Android Build Container..."
docker build -t chatter-android-builder -f Dockerfile.android .

echo "Starting Android Build for ARM64 (aarch64-linux-android)..."
# Mount the project directory and run the build inside the container
docker run --rm \
    -v "$(pwd)":/app \
    -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
    -v "$HOME/.cargo/git":/usr/local/cargo/git \
    chatter-android-builder

echo "---------------------------------------------------"
echo "Build complete! Your APK should be located at:"
echo "./target/release/apk/chatter-linux.apk"
echo "---------------------------------------------------"
