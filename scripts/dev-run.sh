#!/usr/bin/env bash
# Dev launcher: no keyring prompts.
#
# Two secrets normally live in the OS keyring. Both have an environment override
# for development and CI:
#
#   CHATTER_DB_PASSPHRASE  SQLCipher passphrase. Must be the SAME one your
#                          existing database was created with, or it will not
#                          open. For a throwaway profile, any value works.
#   CHATTER_MESH_SEAL_KEY  Mesh outbox seal key, 64 hex chars. Any fixed value
#                          is fine for dev; changing it makes previously sealed
#                          mesh blobs unreadable (they are then treated as
#                          corrupt, not absent).
#
# Development only — these are plaintext keys in the environment.
set -euo pipefail
cd /home/alex/Documents/chatter/linux-client

if [[ -z "${CHATTER_DB_PASSPHRASE:-}" ]]; then
  echo "Set CHATTER_DB_PASSPHRASE to your existing database passphrase first." >&2
  echo "  export CHATTER_DB_PASSPHRASE='...'" >&2
  echo "(Or point XDG_DATA_HOME at a scratch dir to start a fresh profile.)" >&2
  exit 1
fi

export CHATTER_MESH_SEAL_KEY="${CHATTER_MESH_SEAL_KEY:-$(printf 'ab%.0s' {1..32})}"
exec ./target/debug/chatter-ui "$@"
