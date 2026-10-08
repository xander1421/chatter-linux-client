#!/usr/bin/env python3
"""
VALIDATED-UI static wiring lint (LAYER 2 — WIRING-STATIC backstop).

Five falsifiable checks over the chatter-ui UI layer.

Checks A/B ask "is this handler connected to anything". C/D/E ask the question
that let the worst regressions through: **can a user reach it at all.** A wire
can be perfectly CORE-WIRED and still be unreachable because the component is
never rendered (C), nothing invokes the callback (D), or the only affordance is
a mouse button the device does not have (E). Every one of those shipped here.


  A) DANGLING MainWindow callback surface.
     Every `callback NAME` declared on the exported `MainWindow` (ui/app.slint)
     is the Rust-facing surface: Slint generates an `on_NAME` / `invoke_NAME`
     for it. If NO `.on_NAME(` handler exists anywhere in the Rust, the
     button/toggle fires into the void. We report every MainWindow callback
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

  C) SLINT IMPORT THAT NEVER RENDERS.
     `import { X } ...` with no `X` anywhere else in that file. Slint does not
     warn — an unused import is legal — so a whole component can exist,
     compile, and never appear. (HARD FAIL, allowlist with a written reason.)

  D) CALLBACK DECLARED AND NEVER INVOKED.
     A `callback foo()` that no Slint expression calls and no Rust
     `invoke_foo(` fires. Binding a handler is NOT invoking it: such a wire
     passes A and B while the user has no way to trigger it.
     (HARD FAIL, allowlist with a written reason.)

  E) RIGHT-CLICK-ONLY CONTEXT MENU.
     A TouchArea whose menu opens only on `PointerEventButton.right`, with no
     hold path. Android has no secondary button and no hover, so the action is
     unreachable there — this is how mobile lost its folder menus. (HARD FAIL.)

Exit non-zero if any of B-STUB / C / D / E finds something outside its
allowlist. LOCAL-ONLY and [A] are reported but do not fail the build.

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

# Top-level declarations only (4-space indent): MainWindow is the only
# component in app.slint.
CB_RE = re.compile(r"^    callback\s+([A-Za-z0-9_\-]+)\s*(\(|;)")


def mainwindow_callbacks():
    """Return the callback names declared on MainWindow."""
    text = read(os.path.join(UI_DIR, "app.slint"))
    return [snake(m.group(1)) for m in map(CB_RE.match, text.splitlines()) if m]


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


# =============================================================================
# C) SLINT IMPORT NEVER REFERENCED
#
# `import { MobileBottomNav } from "components/mobile_bottom_nav.slint";` in
# app.slint with no `MobileBottomNav { ... }` anywhere in that file is a whole
# component that exists, compiles, and never renders. Slint does not warn: an
# unused import is legal. This is the cheapest possible detector for the
# "declared but never instantiated" class, and it is exact — no heuristics.
# =============================================================================

SLINT_IMPORT_RE = re.compile(
    r"^\s*import\s*\{([^}]*)\}\s*from\s*\"([^\"]+)\"\s*;", re.M)

# Imports that are dead ON PURPOSE, each with the reason. An entry here is a
# claim someone has to defend; it is not a way to make the gate quiet.
KNOWN_DEAD_IMPORTS = {
    # A whole mobile navigation bar (ui/components/mobile_bottom_nav.slint,
    # 5 tabs + 4 callbacks) that app.slint imports and never instantiates.
    # Left in place deliberately: deleting the import would delete the only
    # evidence, and instantiating it is a product decision, not a lint fix.
    # Its `calls-tab-clicked` callback shows up dead in check [D] for the same
    # reason. Owner call: render it or delete the component.
    ("ui/app.slint", "MobileBottomNav"),
}


def all_slint_files():
    for dp, _dn, fn in os.walk(UI_DIR):
        for f in sorted(fn):
            if f.endswith(".slint"):
                yield os.path.join(dp, f)


def unused_slint_imports(slint_texts):
    out = []
    for path, text in slint_texts.items():
        rel = os.path.relpath(path, ROOT)
        for m in SLINT_IMPORT_RE.finditer(text):
            rest = text[:m.start()] + text[m.end():]
            for raw in m.group(1).split(","):
                raw = raw.strip()
                if not raw:
                    continue
                name = raw.split()[-1]  # handles `X as Y`
                if not re.search(r"\b" + re.escape(name) + r"\b", rest):
                    out.append((rel, name, text[:m.start()].count("\n") + 1))
    return out


# =============================================================================
# D) CALLBACK DECLARED AND NEVER INVOKED
#
# A `callback foo()` that nothing ever CALLS is a wire with no trigger. The
# Rust side can register `.on_foo` and pass every check in [A]/[B] while the
# user has no way to reach it — `friends.search-loading` and
# `create-section-loading` were found exactly this way, by hand, after shipping.
# Binding a handler (`foo => {...}`) is NOT an invocation; only `foo(...)` in
# Slint or `invoke_foo(` in Rust is.
# =============================================================================

CB_DECL_RE = re.compile(
    r"^[ \t]*(?:pure\s+)?callback\s+([A-Za-z0-9_\-]+)\s*(?:\(|;|<=>|->)", re.M)

# Dead callbacks that are KNOWN and triaged. Each line is a finding, not an
# exemption — the gate stays green so a NEW dead callback stands out.
KNOWN_DEAD_CALLBACKS = {
    # MobileBottomNav's "Calls" tab. Dead because the nav bar itself is never
    # instantiated — see KNOWN_DEAD_IMPORTS.
    "calls-tab-clicked",
    # Declared on MainWindow and handled in Rust
    # (features/calls/device_picker.rs) but no Slint element calls it: the
    # audio OUTPUT picker has no affordance. The INPUT picker
    # (`show-audio-device-picker`) does. Half a feature.
    "show-audio-output-picker",
    # `pure callback get-user-status() -> string` with a Rust handler
    # (features/status.rs) that nothing queries.
    "get-user-status",
    # UnifiedSearch raises nothing when it opens, so `on_unified_search_opened`
    # never runs and the palette cannot prefetch.
    "opened",
    # ThreadPanel declares `load-more-replies` and app.slint binds it to
    # `load-more-thread-replies`, but the panel has no "load more" affordance —
    # paging is unreachable past the first page.
    "load-more-replies",
    # AttachmentTray (ui/components/attachment_tray.slint) is exported from
    # components/mod.slint and instantiated nowhere, so both of its callbacks
    # are dead by containment.
    "item-clicked",
    "remove-clicked",
}


def dead_callbacks(slint_texts, rust_text):
    """Callbacks declared somewhere in ui/** that nothing ever invokes."""
    decls = {}
    for path, text in slint_texts.items():
        for m in CB_DECL_RE.finditer(text):
            decls.setdefault(m.group(1), (os.path.relpath(path, ROOT),
                                          text[:m.start()].count("\n") + 1))
    dead = []
    for name, site in sorted(decls.items()):
        pat = re.compile(r"(?<![\w\-])" + re.escape(name) + r"\s*\(")
        invoked = False
        for text in slint_texts.values():
            for m in pat.finditer(text):
                head = text[text.rfind("\n", 0, m.start()) + 1:m.start()]
                if re.search(r"\bcallback\s+$", head):
                    continue  # the declaration itself
                invoked = True
                break
            if invoked:
                break
        if not invoked and re.search(
                r"\binvoke_" + re.escape(snake(name)) + r"\s*\(", rust_text):
            invoked = True
        if not invoked:
            dead.append((name, site))
    return dead


# =============================================================================
# E) RIGHT-CLICK-ONLY CONTEXT MENU  (touch-unreachable action)
#
# Android has no secondary mouse button and no hover. A `pointer-event` that
# only opens its menu on `PointerEventButton.right` is invisible on a phone —
# this is how the sidebar folder menus became read-only on mobile.
# `ui/components/hold_area.slint` is the fix: it raises ONE `held()` callback
# from either a 500ms hold or a right-click.
#
# The rule: any TouchArea that tests `PointerEventButton.right` must ALSO have a
# touch path — it is a `HoldArea`, or it arms a hold timer (`... .running =
# true`) on pointer-down, directly or through component-local helpers, and
# its expiry reaches the same menu action. A row that copies only right-click
# branch fails here.
# =============================================================================

RIGHT_BTN = "PointerEventButton.right"
# `foo := TouchArea {`, `TouchArea {`, `component X inherits TouchArea {`, and
# the HoldArea forms of each.
TOUCH_HEADER_RE = re.compile(r"(?:inherits\s+|:=\s*|^\s*)(TouchArea|HoldArea)\s*\{\s*$")


def enclosing_touch_block(text: str, idx: int):
    """Walk out from `idx` to the nearest enclosing TouchArea/HoldArea block.
    Returns (kind, header_line_no, body) or None."""
    depth = 0
    i = idx
    while i > 0:
        i -= 1
        c = text[i]
        if c == "}":
            depth += 1
        elif c == "{":
            if depth == 0:
                head = text[text.rfind("\n", 0, i) + 1:i + 1]
                m = TOUCH_HEADER_RE.search(head.rstrip() + "\n")
                if m:
                    return (m.group(1), text[:i].count("\n") + 1,
                            balanced_block(text, i))
                depth = 0  # keep climbing: a plain Rectangle/layout in between
            else:
                depth -= 1
    return None


def slint_code(text):
    """Mask comments and strings without moving braces or source locations."""
    tokens = re.compile(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"', re.S)
    return tokens.sub(lambda m: re.sub(r'[^\n]', ' ', m.group()), text)


def enclosing_component(text, idx):
    depth = 0
    for i in range(idx - 1, -1, -1):
        if text[i] == '}':
            depth += 1
        elif text[i] == '{':
            if depth:
                depth -= 1
            else:
                head = text[text.rfind('\n', 0, i) + 1:i]
                if re.search(r'\bcomponent\s+[\w-]+(?:\s+inherits\s+[\w-]+)?\s*$', head):
                    return balanced_block(text, i)
    return ''


ROOT_CALL_RE = re.compile(r'\broot\.([\w-]+)\s*\(')


def helper_hold_path(text, idx, body):
    """Resolve component-local press helpers to a timer and the menu action.

    Only calls from a pointer-down branch count. Timer expiry must reach an
    action also reachable from the right-click branch; an unrelated timer or
    an uncalled helper does not establish a touch menu path.
    """
    scope = enclosing_component(text, idx)
    functions = {}
    for m in re.finditer(r'\bfunction\s+([\w-]+)\s*\([^)]*\)\s*(?:->\s*[\w<>-]+\s*)?\{', scope):
        functions[m.group(1)] = balanced_block(scope, m.end() - 1)

    def expand(code, visited=None):
        visited = set() if visited is None else visited
        result = code
        for name in ROOT_CALL_RE.findall(code):
            if name in functions and name not in visited:
                visited.add(name)
                result += '\n' + expand(functions[name], visited)
        return result

    def branches(code, guards=()):
        # Balanced braces keep nested ifs separate; carry outer guards so a
        # down test nested inside a right-button guard cannot look touchable.
        cursor = 0
        pattern = re.compile(r'\bif\s*([^{};]+)\{')
        while (m := pattern.search(code, cursor)):
            inner = balanced_block(code, m.end() - 1)
            conditions = guards + (m.group(1),)
            # Do not attribute a nested, separately guarded call to its parent.
            direct = inner
            nested = list(pattern.finditer(inner))
            spans = []
            end = 0
            for child in nested:
                if child.start() < end:
                    continue
                child_body = balanced_block(inner, child.end() - 1)
                end = child.end() + len(child_body) + 1
                spans.append((child.start(), end))
            for start, end in reversed(spans):
                direct = direct[:start] + ' ' * (end - start) + direct[end:]
            yield conditions, direct
            yield from branches(inner, conditions)
            cursor = m.end() + len(inner) + 1

    handler = re.search(r'\bpointer-event\s*\([^)]*\)\s*=>\s*\{', body)
    if not handler:
        return False
    pointer = balanced_block(body, handler.end() - 1)
    down, right = [], []
    for guards, code in branches(pointer):
        condition = ' '.join(guards)
        if re.search(r'\b\w+\.kind\s*==\s*PointerEventKind\.down\b', condition) and RIGHT_BTN not in condition:
            down.append(expand(code))
        if re.search(r'\b\w+\.button\s*==\s*PointerEventButton\.right\b', condition):
            right.append(expand(code))
    menu_calls = set(ROOT_CALL_RE.findall('\n'.join(right))) - functions.keys()
    armed = set(re.findall(r'\b([\w-]+)\.running\s*=\s*true\b', '\n'.join(down)))
    for m in re.finditer(r'\b([\w-]+)\s*:=\s*Timer\s*\{', scope):
        if m.group(1) not in armed:
            continue
        timer = balanced_block(scope, m.end() - 1)
        triggered = re.search(r'\btriggered\s*=>\s*\{', timer)
        if triggered:
            action = expand(balanced_block(timer, triggered.end() - 1))
            if menu_calls & set(ROOT_CALL_RE.findall(action)):
                return True
    return False


def right_click_only_areas(slint_texts):
    out = []
    for path, text in slint_texts.items():
        text = slint_code(text)
        rel = os.path.relpath(path, ROOT)
        seen = set()
        for m in re.finditer(re.escape(RIGHT_BTN), text):
            blk = enclosing_touch_block(text, m.start())
            if blk is None:
                out.append((rel, m.start() and text[:m.start()].count("\n") + 1,
                            "no enclosing TouchArea"))
                continue
            kind, line, body = blk
            if line in seen:
                continue
            seen.add(line)
            if kind == "HoldArea":
                continue                      # HoldArea IS the touch path
            if helper_hold_path(text, m.start(), body):
                continue                      # rolls its own hold timer
            out.append((rel, line, "TouchArea with right-click and no hold path"))
    return out


def main():
    as_json = "--json" in sys.argv

    rust_texts = [read(p) for p in all_rust_files()]
    slint_texts = {p: read(p) for p in all_slint_files()}
    all_rust = "\n".join(rust_texts)
    fn_bodies = index_fn_bodies(rust_texts)

    # Collect all Rust handlers across the tree, keep the "richest" classification
    # per name (a name may be registered once; if multiple, prefer CORE-WIRED).
    rank = {"STUB": 0, "LOCAL-ONLY": 1, "FN-REF": 2, "CORE-WIRED": 3}
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
    for name in mw:
        if name not in handler_class:
            no_handler.append(name)

    all_stubs = sorted([n for n, c in handler_class.items() if c == "STUB"])
    stubs = [n for n in all_stubs if n not in KNOWN_INTENTIONAL_STUBS]
    known_stubs = [n for n in all_stubs if n in KNOWN_INTENTIONAL_STUBS]
    local_only = sorted([n for n, c in handler_class.items() if c == "LOCAL-ONLY"])
    core_wired = sorted([n for n, c in handler_class.items() if c == "CORE-WIRED"])
    fn_refs = sorted([n for n, c in handler_class.items() if c == "FN-REF"])

    # C) imports that render nothing
    all_imports = unused_slint_imports(slint_texts)
    dead_imports = [(f, n, l) for f, n, l in all_imports
                    if (f, n) not in KNOWN_DEAD_IMPORTS]
    known_imports = [(f, n, l) for f, n, l in all_imports
                     if (f, n) in KNOWN_DEAD_IMPORTS]

    # D) callbacks nothing invokes
    all_dead_cb = dead_callbacks(slint_texts, all_rust)
    dead_cb = [(n, s) for n, s in all_dead_cb if n not in KNOWN_DEAD_CALLBACKS]
    known_cb = [(n, s) for n, s in all_dead_cb if n in KNOWN_DEAD_CALLBACKS]

    # E) menus a finger cannot open
    touch_gaps = right_click_only_areas(slint_texts)

    if as_json:
        print(json.dumps({
            "mainwindow_callbacks": len(mw),
            "rust_handlers": len(handler_class),
            "mainwindow_no_rust_handler": no_handler,
            "stub_handlers": stubs,
            "local_only_handlers": local_only,
            "fn_ref_handlers": fn_refs,
            "core_wired_count": len(core_wired),
            "unused_slint_imports": dead_imports,
            "dead_callbacks": [n for n, _ in dead_cb],
            "right_click_only_touchareas": touch_gaps,
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
        for name in sorted(no_handler):
            print(f"      - {name}")
        print()
        print(f"[B] LOCAL-ONLY handlers (never touch core ctx — eyeball): {len(local_only)}")
        for n in local_only:
            print(f"      - {n}  ({handler_file[n]})")
        print()
        print(f"[B] NEW STUB handlers (HARD FAIL): {len(stubs)}")
        for n in stubs:
            print(f"      - {n}  ({handler_file[n]})")
        print()
        print(f"[C] Slint imports never referenced — component never renders")
        print(f"    known/triaged (allow): {len(known_imports)}")
        for f, n, l in known_imports:
            print(f"      - {n}  ({f}:{l})")
        print(f"    NEW (HARD FAIL): {len(dead_imports)}")
        for f, n, l in dead_imports:
            print(f"      - {n}  ({f}:{l})")
        print()
        print(f"[D] Callbacks declared and NEVER invoked (no trigger exists)")
        print(f"    known/triaged (allow): {len(known_cb)}")
        for n, (f, l) in known_cb:
            print(f"      - {n}  ({f}:{l})")
        print(f"    NEW (HARD FAIL): {len(dead_cb)}")
        for n, (f, l) in dead_cb:
            print(f"      - {n}  ({f}:{l})")
        print()
        print(f"[E] Right-click-only menus (unreachable on touch) (HARD FAIL): "
              f"{len(touch_gaps)}")
        for f, l, why in touch_gaps:
            print(f"      - {f}:{l}  {why}")
        if touch_gaps:
            print("    Fix: use HoldArea (ui/components/hold_area.slint) — it")
            print("    raises one `held()` from a hold OR a right-click.")
        print("=" * 70)

    return 1 if (stubs or dead_imports or dead_cb or touch_gaps) else 0


if __name__ == "__main__":
    sys.exit(main())
