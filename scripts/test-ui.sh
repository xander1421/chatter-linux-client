#!/usr/bin/env bash
#
# VALIDATED-UI regression gate — the UI analog of `make test-validated`.
#
# Runs BOTH proof layers in one shot:
#   LAYER 1 (headless-dynamic): every chatter-ui test target — the lib tests
#       and the slint::testing suites in crates/chatter-ui/tests/ (ui_wiring
#       boots the real MainWindow under the headless testing backend, drives
#       real interactions, and asserts the bound property/core state changed
#       after the round-trip; the others pin layout, rendering and regressions).
#   LAYER 2 (wiring-static): scripts/ui-wiring-lint.py — flags dangling
#       MainWindow callbacks, LOCAL-ONLY handlers, STUB (dead) handlers,
#       imports that never render, callbacks nothing invokes, and right-click
#       menus a finger cannot open.
#
# The ui-test-debug feature emits Slint metadata for ElementHandle queries.
# Ordinary builds omit it; Cargo tracks the separate generated-code variants.
#
# --features chatter-core/insecure-test-anchors is REQUIRED: integration tests
# link WITHOUT `cfg(test)` set on chatter-core, so its `cfg!(test)` dev-key
# fallback (crates/chatter-core/src/config.rs) does not apply — the four pinned
# trust anchors are then unconfigured and the build stops. That feature is the
# named, dev-profile-only carve-out for exactly this; chatter-core's build.rs
# refuses it in a release profile, so it cannot reach a shippable artifact.
# (dev-trust-root alone is not enough: it covers the three key anchors, never
# the TLS CA, which has no dev/prod split.)
#
# Usage:  ./scripts/test-ui.sh            (from the linux-client submodule root)
# Exit non-zero if any layer fails.

set -uo pipefail
cd "$(dirname "$0")/.."   # linux-client/

echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 0 — dev-credential gate"
echo "=================================================================="
# Cheapest layer, so it runs first: the dev auto-register path (fixed password
# and PIN, both public in this repo) must be behind a compile-time cargo
# feature, not a runtime `cfg!(target_os = "android")` test that is true for
# release APKs too. See scripts/dev-gate-lint.py.
python3 scripts/dev-gate-lint.py
DEVGATE_RC=$?

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 2 — static wiring lint"
echo "=================================================================="
python3 scripts/ui-wiring-lint.py
LINT_RC=$?

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 1 — every chatter-ui test target, headless"
echo "=================================================================="
# The lib tests and all integration targets (tests/*.rs): ui_wiring, the
# ui_layout geometry suite, the ui_render smoke (is anything ON SCREEN), and
# the per-regression binaries. Each target is its own process, which the
# integration testing backend needs (it is once-per-process). The debug-info
# flag changes codegen, so the first run rebuilds; later runs are incremental.
# chatter-core/testing widens the service accessors reaction_snapshot uses.
# Filter the benign per-query debug-info notice.
FEATURES=ui-test-debug,chatter-core/dev-trust-root,chatter-core/insecure-test-anchors,chatter-core/testing

CHATTER_NO_KEYRING=1 cargo test -j 2 -p chatter-ui --features $FEATURES --tests --no-fail-fast 2>&1 \
    | grep -v 'requires the presence of debug info'
TEST_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " RESULT"
echo "=================================================================="
echo "  dev-credential gate: $([ $DEVGATE_RC -eq 0 ] && echo PASS || echo 'FAIL (dev creds would ship)')"
echo "  static wiring lint : $([ $LINT_RC -eq 0 ] && echo PASS || echo 'FAIL (stub/dead wire)')"
echo "  chatter-ui tests   : $([ $TEST_RC -eq 0 ] && echo PASS || echo FAIL)"

# Everything the lint reports as "known" is allowlisted with a written reason in
# scripts/ui-wiring-lint.py — 3 code-preview no-ops, 1 never-rendered import,
# 7 never-invoked callbacks. Those allowlists are the positive control: they are
# real findings the gate CAN see, held green so a NEW one stands out.
[ $TEST_RC -eq 0 ] && [ $LINT_RC -eq 0 ] && [ $DEVGATE_RC -eq 0 ]
