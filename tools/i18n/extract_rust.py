#!/usr/bin/env python3
"""Extract chatter_i18n::tr/trf/trn string literals from Rust into a .pot.

Usage: tools/i18n/extract_rust.py > crates/chatter-i18n/translations/chatter-rust.pot
"""
import pathlib, re, sys

LIT = r'"((?:[^"\\]|\\.)*)"'
CALL = re.compile(r'\btr(n|fc|f|c)?\(\s*' + LIT + r'(?:\s*,\s*' + LIT + r')?', re.S)
entries = {}  # (msgid, plural) -> [locations]
for root in ("crates/chatter-ui/src", "crates/chatter-core/src"):
    for path in sorted(pathlib.Path(root).rglob("*.rs")):
        text = path.read_text(encoding="utf-8")
        for m in CALL.finditer(text):
            kind, msgid, second = m.groups()
            # A Rust line continuation (backslash-newline + indent) is not part of the text.
            def clean(s):
                s = re.sub(r"\\\n\s*", "", s)
                s = re.sub(r"\\u\{([0-9a-fA-F]+)\}", lambda u: chr(int(u.group(1), 16)), s)
                # Rust escapes -> text -> gettext escapes.
                s = re.sub(r"\\(.)", lambda e: {"n": "\n", "t": "\t"}.get(e.group(1), e.group(1)), s)
                return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\t", "\\t")
            context = None
            if kind in ("c", "fc"):
                context, msgid, second = msgid, second, None
            key = (clean(msgid), clean(second) if kind == "n" and second is not None else None, context)
            line = text.count("\n", 0, m.start()) + 1
            entries.setdefault(key, []).append(f"{path}:{line}")

out = ['msgid ""', 'msgstr ""', '"Content-Type: text/plain; charset=UTF-8\\n"', ""]
for (msgid, plural, context), where in sorted(entries.items(), key=lambda e: e[1][0]):
    out.append("#: " + " ".join(where))
    if context:
        out.append(f'msgctxt "{context}"')
    out.append(f'msgid "{msgid}"')
    if plural is None:
        out.append('msgstr ""')
    else:
        out += [f'msgid_plural "{plural}"', 'msgstr[0] ""', 'msgstr[1] ""']
    out.append("")
sys.stdout.write("\n".join(out))
