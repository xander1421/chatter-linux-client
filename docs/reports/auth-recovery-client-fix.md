# RC101 client session-ended recovery (S3)

Implemented in the client checkout based on `c5c00bd8`. Server S2 status classification and fatal LoginFinish credential persistence remain the server worker's dependency; this change must ship with those fixes.

## Behavior

- The shared authentication interceptor stamps device Basic/Bearer requests with an in-process credential generation. The shared transport observes actual gRPC `Unauthenticated` status in response headers and trailers, including long-lived streaming trailers. Transport errors and `Unavailable` do not trigger recovery.
- All 25 current `ProductionPublicMethods` procedures are explicitly excluded, including signup, LoginStart/LoginFinish, recovery-code proofs, anonymous mailbox/attachments, new-device provisioning and public KT proofs. Public credential rejection cannot terminate an existing session even if the shared interceptor attached Basic credentials.
- The callback uses a weak client reference and schedules teardown outside session-owned tasks. Under the existing auth lifecycle lock it verifies the rejected credential generation is still current. Credential rotation, login and explicit logout advance the generation; duplicate failures and late replies from previous credentials become no-ops. The teardown captures the current account revision under that lock and uses existing revision-fenced shutdown.
- Before invalidating pending work, recovery flushes existing composer drafts and staged/parked recovery using existing account-scoped persistence. It does not call destructive `clear_composer_session` or normal logout.
- Recovery clears the interceptor and persisted device token plus the PIN fingerprint, stops session tasks/inbox/sync/notifications/calls through existing shutdown, disconnects, and publishes an explicit reactive session-ended state. Persisted CURRENT_USER_ID and CURRENT_DEVICE_ID, identity/profile keys, history, drafts and local database remain. Token presence is now required by the local logged-in check, so restarting after expiry does not start session loops from a retained user ID alone.
- Native UI observes the session-ended state and routes to the existing login/password/PIN recovery screen with an explanatory message. The queued routing closure rechecks the account revision; the existing login success closure now does the same, preventing a delayed success from overriding a subsequent ended session.
- Existing password/PIN/OPAQUE login remains mandatory and sends the retained device ID. No stored-password auto-login, reset, automatic logout or relink was added. `needs_prekey_upload` compares the stored ID with the actual server-returned ID, covering replacement of a revoked/missing device.

## Source-only verification

Read the recovery review and traced current login, restore, interceptor, transport, session shutdown, draft persistence and native UI routing. Checked installed tonic source: InterceptedService preserves request extensions when recreating the HTTP request, and Status::from_header_map is public. Formatted only owned Rust sources and ran `git diff --check`; no builds, tests, application/device runs, secrets or database access were performed.

All six Saved deletion outbox source files from `c5c00bd8` remain untouched. No server source changed. A committed copy of this report is `docs/reports/auth-recovery-client-fix.md` in the client repository; the requested coordinator artifact is `/home/alex/.cache/chatter-rc101/reports/auth-recovery-client-fix.md`.

## Remaining validation / limits

Root owns compilation and runtime validation after source workers and review finish. Exercise header-only unary rejection, stream trailers, simultaneous failures, login versus late old-stream rejection, public bad-password/PIN rejection, Unavailable/transport retry, preserved drafts/history/keys, same-device reauthentication, and server-assigned replacement ID prekey upload. The duplicated public procedure list must follow future server public-surface changes.

If local credential deletion fails, the error is logged, the interceptor still clears, loops still stop and the in-memory session-ended state still routes to recovery; persistence across restart cannot be guaranteed when storage itself rejects writes. Existing explicit user logout semantics are unchanged. This work does not renew server sessions or recover missing server device rows automatically.
