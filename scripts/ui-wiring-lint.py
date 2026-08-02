#!/usr/bin/env python3
"""
VALIDATED-UI static wiring lint (LAYER 2 — WIRING-STATIC backstop).

Two falsifiable checks over the chatter-ui UI layer:

  A) DANGLING MainWindow callback surface.
     Every `callback NAME` declared on the exported `MainWindow` (ui/main.slint)
     is the Rust-facing surface: Slint generates an `on_NAME` / `invoke_NAME`
     for it. If NO `.on_NAME(` handler exists anywhere in the Rust and the
     callback isn't a pure `<=> app.x` forward that another element handles,
     the button/toggle fires into the void. We report every MainWindow callback
     with no Rust `.on_` handler so the ledger can triage each as
     handled-in-Slint vs genuinely dead.

  B) STUB / LOCAL-ONLY handlers.
     For every `.on_NAME(` handler in Rust we brace-balance the closure body
     and classify it:
       - STUB      : empty, or only `todo!()` / `unimplemented!()` / comments.
                     (HARD FAIL — a registered-but-dead wire.)
       - LOCAL-ONLY: never references a core context ident (ui_ctx/ctx/client).
                     Touches only the Slint window. This is the exact bug class
                     "toggle wired to a local bool that never hit core" — but
                     some are legitimately UI-only (open a modal, toggle a
                     panel), so it's a WARN the ledger must eyeball, not a fail.
       - CORE-WIRED: references a core context ident (reaches chatter-core).
       - FN-REF    : registered by function reference, not a closure
                     (`.on_x(platform::android::haptics::long_press)`). The
                     body is resolved and classified where it is DEFINED.
                     Before this existed the classifier saw "no closure block"
                     and reported STUB, which is why the gate was red on master
                     for two correct Android JNI bindings.

Exit non-zero if any STUB is found (regression gate). LOCAL-ONLY is reported
but does not fail the build.

Usage:  python3 scripts/ui-wiring-lint.py [--json]
Run from the linux-client submodule root (or anywhere; paths are resolved
relative to this script).
"""
import os
import re
import sys
import json

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)  # linux-client/
UI_DIR = os.path.join(ROOT, "ui")
SRC_DIR = os.path.join(ROOT, "crates", "chatter-ui", "src")

CORE_IDENTS = ("ui_ctx", "ctx", "client", "_ctx", "core", "svc", "service")

# Handlers that are DELIBERATE, documented no-ops (not regressions). Text
# selection in the code-preview panel needs a selection renderer that is not
# wired; the pointer handlers are intentionally empty and copy-selection falls
# back to copying the full file (see VALIDATED-UI.md finding F-UI-6). Allowlisted
# so the gate stays green for known gaps while catching any NEW dead wire.
KNOWN_INTENTIONAL_STUBS = {
    "code_preview_pointer_down",
    "code_preview_pointer_move",
    "code_preview_pointer_up",
}


def snake(name: str) -> str:
    return name.replace("-", "_")


def read(path: str) -> str:
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        return f.read()


def all_rust_files():
    for dp, _dn, fn in os.walk(SRC_DIR):
        for f in fn:
            if f.endswith(".rs"):
                yield os.path.join(dp, f)


# ---- A) MainWindow callback surface -----------------------------------------

CB_RE = re.compile(r"^\s*callback\s+([A-Za-z0-9_\-]+)\s*(<=>|\(|;)")


def mainwindow_callbacks():
    """Return list of (name, forwarded_bool) declared on MainWindow."""
    text = read(os.path.join(UI_DIR, "main.slint"))
    out = []
    for line in text.splitlines():
        m = CB_RE.match(line)
        if m:
            name = snake(m.group(1))
            forwarded = "<=>" in line
            out.append((name, forwarded))
    return out


# ---- B) Rust handler bodies --------------------------------------------------

ON_RE = re.compile(r"\.on_([a-z0-9_]+)\s*\(")


def extract_handlers(rust_text: str):
    """Yield (name, body, reg_expr) for each `.on_NAME(` by brace-balancing to
    the close of the closure/call. Heuristic but robust enough for
    classification.

    `body` is the closure body, or **None** when the registration passes a
    function *reference* instead of a closure (`.on_x(some::path::fn)`). None
    and "" are deliberately different: an empty closure is a dead wire, a
    function reference is a live one whose body lives elsewhere."""

    for m in ON_RE.finditer(rust_text):
        name = m.group(1)
        i = m.end()  # just after '('
        # Find start of closure body: first '{' after a '|...|' or immediate.
        # We balance from the opening '(' to its matching ')', then look for the
        # closure block inside. Simpler: balance the '(' and take everything as
        # the registration expression, then isolate the outermost {..} block.
        depth = 1
        j = i
        n = len(rust_text)
        in_str = None
        while j < n and depth > 0:
            c = rust_text[j]
            if in_str:
                if c == "\\":
                    j += 2
                    continue
                if c == in_str:
                    in_str = None
            else:
                if c in ("\"", "'"):
                    in_str = c
                elif c == "(":
                    depth += 1
                elif c == ")":
                    depth -= 1
            j += 1
        reg_expr = rust_text[i:j]  # inside the on_X( ... )
        # Isolate the closure body: first '{' to its matching '}'.
        b0 = reg_expr.find("{")
        body = None
        if b0 != -1:
            d = 1
            k = b0 + 1
            s = None
            while k < len(reg_expr) and d > 0:
                c = reg_expr[k]
                if s:
                    if c == "\\":
                        k += 2
                        continue
                    if c == s:
                        s = None
                else:
                    if c in ("\"", "'"):
                        s = c
                    elif c == "{":
                        d += 1
                    elif c == "}":
                        d -= 1
                k += 1
            body = reg_expr[b0 + 1:k - 1]
        yield name, body, reg_expr.strip()


def strip_comments(body: str) -> str:
    body = re.sub(r"//[^\n]*", "", body)
    body = re.sub(r"/\*.*?\*/", "", body, flags=re.DOTALL)
    return body


# A method/field access on a variable whose name STARTS WITH a core-context
# prefix — catches `ui_ctx.foo()` AND the ubiquitous suffixed clones the code
# uses to move a client into a closure (`ui_ctx_read.set_...`, `ui_ctx_ai.set_...`,
# `ctx.bar()`). The prefix must be followed by word chars and then `.`; a bare
# mention with no call does not count.
CORE_CALL_RE = re.compile(r"\b(?:ui_ctx|ctx|client|core|svc|service)\w*\s*\.")

# `fn NAME(` / `pub fn NAME(` — used to resolve function-REFERENCE registrations
# (`.on_x(platform::android::haptics::long_press)`) to the body that actually
# runs. Without this the classifier saw "no closure block" and reported STUB,
# which is how the gate went red on two correct `#[cfg(target_os = "android")]`
# JNI bindings: the wire is live, the body just is not inline.
FN_DEF_RE = re.compile(r"\bfn\s+([a-z0-9_]+)\s*(?:<[^>]*>)?\s*\(")
# A registration argument that is a bare path — `a::b::c` or `foo` — and nothing
# else. Anything with a `{`, `|` or `(` in it is a closure or a call, not a ref.
FN_REF_RE = re.compile(r"^(?:[A-Za-z_]\w*::)*([a-z0-9_]+)$")


def balanced_block(text: str, open_idx: int) -> str:
    """Return the contents of the `{...}` block whose `{` is at `open_idx`."""
    d = 1
    k = open_idx + 1
    s = None
    while k < len(text) and d > 0:
        c = text[k]
        if s:
            if c == "\\":
                k += 2
                continue
            if c == s:
                s = None
        else:
            if c in ("\"", "'"):
                s = c
            elif c == "{":
                d += 1
            elif c == "}":
                d -= 1
        k += 1
    return text[open_idx + 1:k - 1]


def index_fn_bodies(rust_texts):
    """name -> body, for every `fn name(...) { ... }` in the crate."""
    out = {}
    for txt in rust_texts:
        for m in FN_DEF_RE.finditer(txt):
            b0 = txt.find("{", m.end())
            if b0 == -1:
                continue
            # Reject a match whose `{` is past the next `fn` (trait decl, etc).
            out.setdefault(m.group(1), balanced_block(txt, b0))
    return out


def classify(body, reg_expr: str = "", fn_bodies=None) -> str:
    if body is None:
        # No closure block: this is `.on_x(EXPR)`. A bare path is a function
        # reference — a LIVE wire whose body is defined elsewhere. Resolve it
        # and classify what actually runs; if it cannot be resolved, say so
        # rather than calling a real binding dead.
        ref = FN_REF_RE.match(strip_comments(reg_expr).strip())
        if ref and fn_bodies is not None and ref.group(1) in fn_bodies:
            return classify(fn_bodies[ref.group(1)], "", fn_bodies)
        if ref:
            return "FN-REF"
        # Not a path and not a closure (a builder chain, a macro): unclassifiable
        # but definitely not empty.
        return "FN-REF" if strip_comments(reg_expr).strip() else "STUB"
    code = strip_comments(body).strip()
    if code == "":
        return "STUB"
    if re.search(r"\b(todo!|unimplemented!)\s*\(", code) and len(re.sub(r"\b(todo!|unimplemented!)\s*\([^)]*\)", "", code).strip()) == 0:
        return "STUB"
    # Reaches core context (a call on ui_ctx / a suffixed clone / ctx / client)?
    if CORE_CALL_RE.search(code):
        return "CORE-WIRED"
    return "LOCAL-ONLY"


def main():
    as_json = "--json" in sys.argv

    # Collect all Rust handlers across the tree, keep the "richest" classification
    # per name (a name may be registered once; if multiple, prefer CORE-WIRED).
    rank = {"STUB": 0, "LOCAL-ONLY": 1, "FN-REF": 2, "CORE-WIRED": 3}
    fn_bodies = index_fn_bodies([read(p) for p in all_rust_files()])
    handler_class = {}
    handler_file = {}
    for path in all_rust_files():
        txt = read(path)
        for name, body, reg in extract_handlers(txt):
            cls = classify(body, reg, fn_bodies)
            if name not in handler_class or rank[cls] > rank[handler_class[name]]:
                handler_class[name] = cls
                handler_file[name] = os.path.relpath(path, ROOT)

    # A) MainWindow surface
    mw = mainwindow_callbacks()
    no_handler = []
    for name, forwarded in mw:
        if name not in handler_class:
            no_handler.append((name, forwarded))

    all_stubs = sorted([n for n, c in handler_class.items() if c == "STUB"])
    stubs = [n for n in all_stubs if n not in KNOWN_INTENTIONAL_STUBS]
    known_stubs = [n for n in all_stubs if n in KNOWN_INTENTIONAL_STUBS]
    local_only = sorted([n for n, c in handler_class.items() if c == "LOCAL-ONLY"])
    core_wired = sorted([n for n, c in handler_class.items() if c == "CORE-WIRED"])
    fn_refs = sorted([n for n, c in handler_class.items() if c == "FN-REF"])

    if as_json:
        print(json.dumps({
            "mainwindow_callbacks": len(mw),
            "rust_handlers": len(handler_class),
            "mainwindow_no_rust_handler": [n for n, _ in no_handler],
            "stub_handlers": stubs,
            "local_only_handlers": local_only,
            "fn_ref_handlers": fn_refs,
            "core_wired_count": len(core_wired),
        }, indent=2))
    else:
        print("=" * 70)
        print("VALIDATED-UI static wiring lint")
        print("=" * 70)
        print(f"MainWindow callbacks declared     : {len(mw)}")
        print(f"Distinct Rust .on_ handlers       : {len(handler_class)}")
        print(f"  CORE-WIRED (reach chatter-core) : {len(core_wired)}")
        print(f"  LOCAL-ONLY (Slint-only, WARN)   : {len(local_only)}")
        print(f"  STUB known-intentional (allow)  : {len(known_stubs)}  {known_stubs}")
        print(f"  STUB NEW (empty/todo, FAIL)     : {len(stubs)}")
        print()
        print(f"[A] MainWindow callbacks with NO Rust .on_ handler: {len(no_handler)}")
        print("    (each is either handled purely in Slint, or a dead wire —")
        print("     triage in the ledger)")
        for name, fwd in sorted(no_handler):
            tag = "forward<=>" if fwd else "declared"
            print(f"      - {name}  [{tag}]")
        print()
        print(f"[B] LOCAL-ONLY handlers (never touch core ctx — eyeball): {len(local_only)}")
        for n in local_only:
            print(f"      - {n}  ({handler_file[n]})")
        print()
        print(f"[B] NEW STUB handlers (HARD FAIL): {len(stubs)}")
        for n in stubs:
            print(f"      - {n}  ({handler_file[n]})")
        print("=" * 70)

    return 1 if stubs else 0


if __name__ == "__main__":
    sys.exit(main())
