#!/usr/bin/env bash
# The packaged runtime is a patched dependency, outside our workspace. Cargo
# cannot run its unit tests with dev-dependencies via `-p`. Test an exact source
# copy in an isolated workspace so the application lockfile stays unchanged.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/target"
scratch="$(mktemp -d "$root/target/slint-runtime-test.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
cp -a "$root/vendor/i-slint-core/." "$scratch/"
printf '\n[workspace]\n' >> "$scratch/Cargo.toml"
# Reuse the application's resolved versions; extra test dependencies resolve
# only in this disposable workspace.
cp "$root/Cargo.lock" "$scratch/Cargo.lock"
# This damage test needs the normal std geometry/runtime path, not Parley text
# layout whose upstream font fixture is omitted from the published crate.
cargo test -j 1 --manifest-path "$scratch/Cargo.toml" \
    --target-dir "$root/target" --no-default-features --features std,unicode \
    --lib partial_renderer::backdrop_damage_retains_unchanged_pixels -- --exact
