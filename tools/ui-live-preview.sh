#!/usr/bin/env bash
# Opt-in UI development only; never enables preview in packaged builds.
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
for arg in "$@"; do
    case "$arg" in
        --release|--profile|--profile=*)
            echo 'Live preview is restricted to the development profile.' >&2
            exit 2
            ;;
    esac
done
cd "$project_dir"
# Use a separate preview account/profile unless the caller explicitly supplies one.
export XDG_DATA_HOME="${XDG_DATA_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/chatter-ui-preview/data}"
export CHATTER_NO_KEYRING=1
unset CARGO_INCREMENTAL
export SLINT_LIVE_PREVIEW=1
exec cargo run -p chatter-ui --bin chatter-ui --features slint/live-preview "$@"
