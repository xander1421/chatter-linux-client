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
#       MainWindow callbacks, LOCAL-ONLY handlers, and STUB (dead) handlers.
#
# SLINT_EMIT_DEBUG_INFO=1 is REQUIRED: the ElementHandle query API is inert
# without Slint debug info compiled into the generated code.
#
# Usage:  ./scripts/test-ui.sh            (from the linux-client submodule root)
# Exit non-zero if either layer fails.

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
SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --test ui_wiring 2>&1 \
    | grep -v 'requires the presence of debug info'
TEST_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " VALIDATED-UI  ·  LAYER 1 — geometry/layout suite"
echo "=================================================================="
# Separate binary: the integration testing backend (real layout passes) is
# once-per-process and ui_wiring already claims it.
SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --test ui_layout 2>&1 \
    | grep -v 'requires the presence of debug info'
LAYOUT_RC=${PIPESTATUS[0]}

echo
echo "=================================================================="
echo " RESULT"
echo "=================================================================="
echo "  static wiring lint : $([ $LINT_RC -eq 0 ] && echo PASS || echo 'FAIL (stub/dead wire)')"
echo "  headless UI suite  : $([ $TEST_RC -eq 0 ] && echo PASS || echo FAIL)"
echo "  layout suite       : $([ $LAYOUT_RC -eq 0 ] && echo PASS || echo FAIL)"

# The lint currently reports the 3 KNOWN-intentional code-preview no-ops as
# STUBs (documented in VALIDATED-UI.md, allowlisted in the lint). If you have
# not allowlisted them, LINT_RC will be 1 by design — that is the positive
# control proving the gate can see a dead wire.
[ $TEST_RC -eq 0 ] && [ $LINT_RC -eq 0 ] && [ $LAYOUT_RC -eq 0 ]
