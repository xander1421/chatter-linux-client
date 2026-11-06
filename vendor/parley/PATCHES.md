# Local parley 0.11.1 patch

On Android, the Skia renderer can paint the space glyph that HarfRust substitutes
for default-ignorable characters, even though HarfRust gives it zero advance.
Set `REMOVE_DEFAULT_IGNORABLES` on the shaping buffer so HarfRust completes
substitution and positioning, then omits those characters' glyphs. The input
text is unchanged, including ZWJ, ZWNJ, and bidi isolates. Other platforms
keep Parley's upstream shaping behavior.
