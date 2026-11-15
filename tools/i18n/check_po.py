#!/usr/bin/env python3
"""Check catalogs: every entry translated, placeholders and edge whitespace kept.

Usage: tools/i18n/check_po.py crates/chatter-i18n/translations/*/LC_MESSAGES/*.po
"""
import re, sys

def entries(path):
    cur, key = {}, None
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        m = re.match(r'(msgid_plural|msgid|msgstr(?:\[\d+\])?) "(.*)"$', line)
        if m:
            key = m.group(1)
            if key == "msgid" and cur:
                yield cur; cur = {}
            cur[key] = m.group(2)
        elif line.startswith('"') and key:
            cur[key] += line[1:-1]
        elif not line.strip():
            key = None
    if cur: yield cur

PH = re.compile(r"\{[^{}]*\}")
bad = 0
for path in sys.argv[1:]:
    for e in entries(path):
        src = e.get("msgid", "")
        if not src: continue
        for k, v in e.items():
            if not k.startswith("msgstr"): continue
            problems = []
            if not v: problems.append("empty")
            want = sorted(PH.findall(src if k in ("msgstr", "msgstr[0]") else e.get("msgid_plural", src)))
            got = sorted(PH.findall(v))
            # A singular form may drop {n} ("1 file" -> "un fișier"); never add or rename.
            if got != want and not (k != "msgstr" and set(got) <= set(want)):
                problems.append(f"placeholders {got} != {want}")
            if (src[:1] == " ") != (v[:1] == " ") or (src[-1:] == " ") != (v[-1:] == " "):
                problems.append("edge space")
            if src.count("\\n") != v.count("\\n"): problems.append("newlines")
            if problems:
                bad += 1
                print(f"{path}: {src!r} {k}: {', '.join(problems)}")
sys.exit(1 if bad else 0)
