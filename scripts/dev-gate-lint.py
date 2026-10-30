#!/usr/bin/env python3
"""Static gate: dev-only credentials must be COMPILED OUT of a shipped build.

The finding this exists for: `crates/chatter-ui/src/lib.rs` gated its dev
auto-register on `cfg!(target_os = "android")` — a *runtime* OS test that is
true for every Android build, release included. A shipped APK therefore
auto-registered an account with the fixed password/PIN literals that sit in this
repository in plain sight.

A runtime gate cannot fix that: the credentials are still in the binary and the
account creation is one flag flip away. The gate has to be a compile-time
`#[cfg(feature = ...)]`, and the feature must not be in `default`.

This lint is the positive control for that. Point it at the pre-fix source and
check 1 fails; point it at the fixed source and everything passes.

Runs in milliseconds and needs no toolchain — it is layer 0 of
`scripts/test-ui.sh`, alongside the existing static wiring lint.

Usage:  python3 scripts/dev-gate-lint.py     (from the linux-client root)
Exit non-zero on any violation.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The gated helper and the feature that switches it on.
FEATURE = "dev-auto-register"
GATE_ATTR = f'#[cfg(feature = "{FEATURE}")]'
HELPER = "dev_auto_register"

# Credentials that must never reach a build which did not explicitly ask for
# them. Kept as fragments so this file is not itself a grep hit for the literal.
CREDENTIALS = ["dev" + "pass123", "dev" + "pin123"]

UI_SRC = ROOT / "crates" / "chatter-ui" / "src"
UI_CARGO = ROOT / "crates" / "chatter-ui" / "Cargo.toml"
LIB_RS = UI_SRC / "lib.rs"

failures: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


def helper_span(src: str) -> tuple[int, int] | None:
    """Byte range of the feature-gated `fn dev_auto_register` item, attribute
    included. Relies on rustfmt's column-0 `}` to close a top-level item."""
    m = re.search(
        r"^" + re.escape(GATE_ATTR) + r"\n(?:pub )?fn " + HELPER + r"\b",
        src,
        re.MULTILINE,
    )
    if not m:
        return None
    end = src.find("\n}\n", m.end())
    if end == -1:
        return None
    return (m.start(), end + 3)


def rust_files() -> list[Path]:
    return sorted(p for p in UI_SRC.rglob("*.rs"))


# ── 1. Every dev credential literal lives inside the feature-gated helper ────
#
# This is the check that fails against the original code: there was no gated
# helper at all, the literals sat inline in `app_init` behind a `cfg!()`.
src = LIB_RS.read_text()
span = helper_span(src)
if span is None:
    fail(
        f"{LIB_RS.relative_to(ROOT)}: no `{GATE_ATTR}`-gated `fn {HELPER}` found.\n"
        f"    The dev auto-register path must be a compile-time-gated item, not a\n"
        f"    runtime branch — otherwise the credentials ship in every binary."
    )

for path in rust_files():
    text = path.read_text()
    for cred in CREDENTIALS:
        for m in re.finditer(re.escape(cred), text):
            line = text.count("\n", 0, m.start()) + 1
            if path != LIB_RS or span is None or not (span[0] < m.start() < span[1]):
                fail(
                    f"{path.relative_to(ROOT)}:{line}: dev credential literal outside the\n"
                    f"    `{GATE_ATTR}` gated `fn {HELPER}`. It would be compiled into\n"
                    f"    builds that never opted in (including release Android)."
                )

# ── 2. No runtime OS test decides whether to use the dev credentials ─────────
if span is not None:
    body = src[span[0] : span[1]]
    for m in re.finditer(r"cfg!\s*\(\s*target_os", body):
        line = src.count("\n", 0, span[0] + m.start()) + 1
        fail(
            f"{LIB_RS.relative_to(ROOT)}:{line}: `cfg!(target_os = ...)` inside {HELPER}.\n"
            f"    A runtime OS test is exactly the original finding: it is true for\n"
            f"    release builds too. Use `#[cfg(target_os = ...)]`."
        )

# ── 3. Every call site carries the gate attribute ───────────────────────────
for path in rust_files():
    text = path.read_text()
    for m in re.finditer(r"^([ \t]*)" + HELPER + r"\s*\(", text, re.MULTILINE):
        line = text.count("\n", 0, m.start()) + 1
        prev = text[:m.start()].rstrip().rsplit("\n", 1)[-1].strip()
        if prev != GATE_ATTR:
            fail(
                f"{path.relative_to(ROOT)}:{line}: call to {HELPER}() is not immediately\n"
                f"    preceded by `{GATE_ATTR}` (found: {prev!r})."
            )

# ── 4. The feature exists and is NOT on by default ──────────────────────────
cargo = UI_CARGO.read_text()
if not re.search(r"^" + re.escape(FEATURE) + r"\s*=", cargo, re.MULTILINE):
    fail(f"{UI_CARGO.relative_to(ROOT)}: feature `{FEATURE}` is not declared.")

m = re.search(r"^default\s*=\s*\[(.*?)\]", cargo, re.MULTILINE | re.DOTALL)
if m is None:
    fail(f"{UI_CARGO.relative_to(ROOT)}: no `default = [...]` feature list found.")
elif FEATURE in m.group(1):
    fail(
        f"{UI_CARGO.relative_to(ROOT)}: `{FEATURE}` is in `default`. It must be an\n"
        f"    explicit opt-in, like `dev-trust-root`."
    )

# ── 5. No build script enables the feature on a release build ───────────────
for script in sorted(ROOT.glob("**/*.sh")):
    if "/target/" in str(script) or "/vendor/" in str(script):
        continue
    text = script.read_text()
    if FEATURE not in text:
        continue
    for i, line in enumerate(text.splitlines(), 1):
        if FEATURE in line and ("--release" in line or "assembleRelease" in line):
            fail(
                f"{script.relative_to(ROOT)}:{i}: `{FEATURE}` enabled on a release build."
            )

# ── Report ──────────────────────────────────────────────────────────────────
print("=" * 66)
print(" dev-credential gate  ·  dev auto-register must be compiled out")
print("=" * 66)
if failures:
    for f in failures:
        print(f"FAIL  {f}")
    print()
    print(f"{len(failures)} violation(s).")
    sys.exit(1)

print("PASS  dev credentials are behind a compile-time cargo feature")
print(f"PASS  `{FEATURE}` is declared and not in `default`")
print("PASS  no runtime target_os test guards them")
print("PASS  no release build script enables the feature")
sys.exit(0)
