# Client threat model

## Key Transparency and device replacement

During password sign-in, the client verifies its own identity entry against the
pinned Key Transparency signing key and the stored append-only log history. A
verified successor carrying a different identity can establish that an
installation was replaced. The client displays the replacement notice before
signing out and clearing the installation's local account data.

The server operator controls the Key Transparency signing key, and the client
has no independent auditor. An operator who can append and sign a consistent
replacement can therefore trigger a local wipe on an affected device at sign-in.
This is an accepted data-loss risk of the replacement flow. The replacement is
committed to the signed history presented to that client. The wipe also removes
that client's locally pinned history and warning state.

PIN recovery verifies the published identity before spending a backup attempt.
The decrypted backup must contain that identity, and a stale backup does not
install an older private key. An explicit identity reset uploads the new key
backup before publishing the replacement identity.
