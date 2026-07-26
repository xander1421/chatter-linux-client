#!/usr/bin/env bash
# Run the desktop client without the Secret Service (KeePassXC) unlock prompt.
#
# `CHATTER_DB_PASSPHRASE_FILE` makes Keyring read the passphrase from
# `chatter.passphrase` next to the database instead of the system keyring. The
# file already holds the *same* passphrase the keyring holds, so the database
# opens exactly as before — this only changes where the key is read from.
#
# The passphrase is plaintext on disk (mode 0600). That is the trade this script
# makes deliberately, for local development only. Delete the file and stop
# setting this variable to go back to the keyring; nothing else needs changing.
set -euo pipefail

export CHATTER_DB_PASSPHRASE_FILE=1
# Auto-register a dev account rather than prompting for one.
export CHATTER_DEV=1

exec cargo run -p chatter-ui "$@"
