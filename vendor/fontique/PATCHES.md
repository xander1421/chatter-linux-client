# Local fontique 0.10.0 patches

Android `fonts.xml` often omits `postScriptName`. The upstream Android backend only
uses that attribute to connect a fallback font to the family found by the font
scanner. On the Samsung S9, the fallback tables were consequently empty for
Hebrew, Arabic, and CJK even though their Noto font files were installed.

`src/backend/android.rs` now builds a `(file path, face index) -> FamilyId` map
from the scanned fonts. A `<font>` without a usable `postScriptName` resolves
through its file name under `$ANDROID_ROOT/fonts`; `index` selects a TTC face,
defaulting to zero. Compact variant entries precede elegant entries in the
script and locale fallback tables, keeping Android UI typography when both are
available. A small XML fixture tests file and TTC face resolution.

The same device message includes U+200B. Parley 0.10 classifies most format
characters as noncontributing to font coverage, then passes the original text
to HarfRust. HarfRust normally replaces default-ignorable characters with the
selected font's zero-advance space glyph, or removes them if that glyph is
unavailable. The exact source of the reported box is therefore unresolved by
source inspection alone. The coordinator's S9 render pass must check U+200B,
U+200C/D, U+FEFF, and U+2060; a remaining box needs a shaping-level fix because
Slint's glyph renderer has no reliable glyph-to-character mapping.
