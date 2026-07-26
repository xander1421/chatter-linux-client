#!/usr/bin/env bash
# Run the desktop client without any Secret Service (KeePassXC) prompt.
#
# Since the `no-keyring` marker exists, this script is no longer what keeps the
# prompt away: dropping a file named `no-keyring` beside the database opts the
# whole data directory out, so a desktop entry, a bare `cargo run`, or a debugger
# all start without a prompt too. This script only mints the key material and
# still works. To go back to the keyring, delete the marker.
#
# Two separate keys go to the OS keyring, and bypassing only one of them still
# leaves a prompt — or, if the keyring is locked rather than merely closed, a
# D-Bus call that never returns:
#
#   the SQLCipher passphrase  ->  CHATTER_DB_PASSPHRASE_FILE=1 reads it from
#                                 `chatter.passphrase` beside the database
#   the mesh seal key         ->  CHATTER_MESH_SEAL_KEY, 32 bytes of hex
#
# Both keys already exist; this only changes where they are read from, so the
# database opens and the outbox unseals exactly as before.
#
# Both are plaintext on disk at mode 0600. That is the trade this script makes
# deliberately, for local development only. To go back to the keyring: stop
# setting these variables. Delete the files too if you want the material gone.
set -euo pipefail

data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/chatter-linux"
seal_key_file="$data_dir/mesh-seal-key.dev"

export CHATTER_DB_PASSPHRASE_FILE=1

# Mint the dev seal key once and keep it. Generating a fresh one per run would
# make every previous run's sealed outbox unreadable — the exact failure the
# keyring path goes out of its way to avoid.
if [ ! -s "$seal_key_file" ]; then
    mkdir -p "$data_dir"
    ( umask 077 && head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$seal_key_file" )
    echo ">> minted a development mesh seal key at $seal_key_file"
fi
CHATTER_MESH_SEAL_KEY="$(cat "$seal_key_file")"
export CHATTER_MESH_SEAL_KEY

# Auto-register a dev account rather than prompting for one.
export CHATTER_DEV=1

# Create the marker if it is missing, so a run started any other way behaves the
# same as this one. `no-keyring` is what the client actually consults; the two
# variables above only cover this process.
if [ ! -e "$data_dir/no-keyring" ]; then
    mkdir -p "$data_dir"
    printf 'Keyring opted out. Delete this file to go back to the OS keyring.\n' \
        > "$data_dir/no-keyring"
    echo ">> opted this data directory out of the OS keyring ($data_dir/no-keyring)"
fi

# `--features mesh` is not optional and is easy to lose: any plain
# `cargo build -p chatter-ui`, and every `cargo test --workspace`, overwrites
# target/debug/chatter-ui with a build that has no mesh feature. The app then
# starts perfectly and the radio simply never comes up, with no error anywhere —
# `#[cfg(feature = "mesh")]` compiled the start call out. Running through cargo
# here means the binary is always rebuilt with it.
#
# To check a binary by hand:  strings target/debug/chatter-ui | grep 'mesh: started'
exec cargo run -p chatter-ui --features mesh "$@"
