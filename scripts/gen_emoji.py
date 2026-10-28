#!/usr/bin/env python3
"""Emit the emoji picker table from Unicode emoji-test.txt.

Usage: scripts/gen_emoji.py emoji-test.txt crates/chatter-core/src/ui/emoji_table.rs
(emoji-test.txt: https://unicode.org/Public/emoji/latest/emoji-test.txt)
"""
import re
import sys

GROUPS = {
    "Smileys & Emotion": 0,
    "People & Body": 1,
    "Animals & Nature": 2,
    "Food & Drink": 3,
    "Travel & Places": 4,
    "Activities": 5,
    "Objects": 6,
    "Symbols": 7,
    "Flags": 8,
}
TONES = ["light skin tone", "medium-light skin tone", "medium skin tone",
         "medium-dark skin tone", "dark skin tone"]

src, out = sys.argv[1], sys.argv[2]
version = ""
group = None
bases = []          # (emoji, name, category)
toned = {}          # base name -> {tone index: emoji}
line_re = re.compile(r"^([0-9A-F ]+?)\s*; fully-qualified\s*# (\S+) E[0-9.]+ (.+)$")
for line in open(src, encoding="utf-8"):
    line = line.rstrip("\n")
    if line.startswith("# Version:"):
        version = line.split(":", 1)[1].strip()
    if line.startswith("# group:"):
        group = GROUPS.get(line.split(":", 1)[1].strip())
        continue
    m = line_re.match(line)
    if not m or group is None:
        continue
    cps, name = m.group(1), m.group(3)
    emoji = "".join(chr(int(c, 16)) for c in cps.split())
    if ": " in name and "skin tone" in name.split(": ", 1)[1]:
        base, mods = name.split(": ", 1)
        parts = mods.split(", ")
        tones = [p for p in parts if p in TONES]
        rest = [p for p in parts if p not in TONES]
        # One tone for the whole sequence ("woman: medium skin tone, red hair"
        # tones "woman: red hair"); mixed tones are not offered.
        if len(tones) == 1:
            key = base + (": " + ", ".join(rest) if rest else "")
            toned.setdefault(key, {})[TONES.index(tones[0])] = emoji
        continue
    bases.append((emoji, name, group))


def lit(s):
    return '"' + "".join(c if 0x20 <= ord(c) < 0x7F and c not in '"\\' else "\\u{%X}" % ord(c) for c in s) + '"'


with open(out, "w", encoding="utf-8") as f:
    f.write("//! Generated from Unicode emoji-test.txt %s by scripts/gen_emoji.py: every\n" % version)
    f.write("//! fully-qualified emoji in Unicode order, its CLDR name, picker category\n")
    f.write("//! (0=Smileys … 8=Flags) and, for skin-tone bases, the five toned forms.\n")
    f.write("//! Do not edit by hand.\n\n")
    f.write("pub(super) static EMOJI_TABLE: &[(&str, &str, u8, Option<[&str; 5]>)] = &[\n")
    for emoji, name, cat in bases:
        tones = toned.get(name)
        t = "None"
        if tones and len(tones) == 5:
            t = "Some([" + ", ".join(lit(tones[i]) for i in range(5)) + "])"
        f.write("    (%s, %s, %d, %s),\n" % (lit(emoji), lit(name), cat, t))
    f.write("];\n")
print(len(bases), "emoji;", sum(1 for e in bases if len(toned.get(e[1], {})) == 5), "toned")
