#!/usr/bin/env bash
#
# VALIDATED-UI regression gate — the UI analog of `make test-validated`.
#
# Runs BOTH proof layers in one shot:
#   LAYER 1 (headless-dynamic): the slint::testing suite in
#       crates/chatter-ui/tests/ui_wiring.rs — boots the real MainWindow under
#       the headless testing backend, drives real interactions, and asserts the
#       bound property/core state changed after the round-trip.
#   LAYER 2 (wiring-static): scripts/ui-wiring-lint.py — flags dangling
#       MainWindow callbacks, LOCAL-ONLY handlers, STUB (dead) handlers,
#       imports that never render, callbacks nothing invokes, and right-click
#       menus a finger cannot open.
#
# SLINT_EMIT_DEBUG_INFO=1 is REQUIRED: the ElementHandle query API is inert
# without Slint debug info compiled into the generated code.
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
echo " VALIDATED-UI  ·  LAYER 2 — static wiring lint"
echo "=================================================================="
python3 scripts/ui-wiring-lint.py
LINT_RC=$?

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 1 — headless slint::testing suite"
echo "=================================================================="
# The debug-info flag changes codegen, so the first run rebuilds; subsequent
# runs are incremental. Filter the benign per-query debug-info notice.
FEATURES=chatter-core/dev-trust-root,chatter-core/insecure-test-anchors

SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --features $FEATURES --test ui_wiring 2>&1 \
    | grep -v 'requires the presence of debug info'
TEST_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 1 — geometry/layout suite"
echo "=================================================================="
# Separate binary: the integration testing backend (real layout passes) is
# once-per-process and ui_wiring already claims it.
SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --features $FEATURES --test ui_layout 2>&1 \
    | grep -v 'requires the presence of debug info'
LAYOUT_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 1 — render smoke (is anything ON SCREEN)"
echo "=================================================================="
# Third binary for the same once-per-process reason. This is the layer that
# answers the question 43/43 green could not: does the main view actually
# render its columns, or is the app a blank screen with live callbacks?
SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --features $FEATURES --test ui_render 2>&1 \
    | grep -v 'requires the presence of debug info'
RENDER_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " RESULT"
echo "=================================================================="
echo "  static wiring lint : $([ $LINT_RC -eq 0 ] && echo PASS || echo 'FAIL (stub/dead wire)')"
echo "  headless UI suite  : $([ $TEST_RC -eq 0 ] && echo PASS || echo FAIL)"
echo "  layout suite       : $([ $LAYOUT_RC -eq 0 ] && echo PASS || echo FAIL)"
echo "  render smoke       : $([ $RENDER_RC -eq 0 ] && echo PASS || echo FAIL)"

# Everything the lint reports as "known" is allowlisted with a written reason in
# scripts/ui-wiring-lint.py — 3 code-preview no-ops, 1 never-rendered import,
# 7 never-invoked callbacks. Those allowlists are the positive control: they are
# real findings the gate CAN see, held green so a NEW one stands out.
[ $TEST_RC -eq 0 ] && [ $LINT_RC -eq 0 ] && [ $LAYOUT_RC -eq 0 ] && [ $RENDER_RC -eq 0 ]
