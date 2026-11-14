# Task: mark the remaining user-visible UI strings for translation

You are working in a Rust + Slint messaging app (this directory is the client
repo). English is the source language. We are adding translations: every
user-visible English string in `ui/**/*.slint` must be wrapped in Slint's
`@tr(...)` macro so it can be translated. A script already wrapped the simple
cases (`text: "Save";` -> `text: @tr("Save");`). Your job is the remaining
lines, which need judgment. Your list of lines is at the end of this file.

## What to wrap
Text a person reads or a screen reader speaks: labels, titles, button text,
placeholders, hints, tooltips, empty-state text, status text, accessible labels
and descriptions, default values of string properties that are shown as text,
label/title/hint fields inside struct or array literals.

## What NOT to wrap (leave exactly as is)
- Identifiers and state values: `"friends"`, `"requests"`, `"auto"`, `"dnd"`,
  `"failed"`, `"primary"`, `"secondary"`, `"right"`, ids, keys, icon or font
  names, URLs, file paths, format strings for dates.
- Any string that is an operand of `==` or `!=` (e.g.
  `root.failure-reason == "Message too long"`): those are compared with values
  produced in Rust and must stay byte-identical.
- Strings with no words: emoji, `"×"`, `"↵"`, `"#"`, `"·"`, numbers.
- Text inside comments.

## How to wrap
- Plain: `"Add a friend"` -> `@tr("Add a friend")`.
- With values, use positional placeholders instead of `+` concatenation, so
  translators can reorder words:
  `"Reconnecting (attempt " + n + ")"` -> `@tr("Reconnecting (attempt {})", n)`
  Two or more values: `@tr("{0} of {1}", a, b)`.
- Slint string interpolation `"Pinned message \{i} of \{n}"` must become
  `@tr("Pinned message {0} of {1}", i, n)` (no `\{...}` inside `@tr`).
- Counted nouns must use Slint's plural form, never a hand-written ternary:
  `n == 1 ? "1 relay" : n + " relays"` ->
  `@tr("{n} relay" | "{n} relays" % n)`
  (inside a plural `@tr`, `{n}` is the count). Keep any surrounding text in
  the same string when it is one sentence:
  `@tr("{n} pinned message" | "{n} pinned messages" % count)`.
- A literal `{` or `}` in the English text must be written `{{` / `}}`.
- When a sentence is built from several pieces, make it ONE translatable
  string with placeholders. Do not translate fragments like `" of "`.
- A prefix symbol that is not language (`"# "`, `"@"`, `"📎 "`) stays outside:
  `"@" + handle` is left alone; `"📎 " + @tr("Attachment")` is fine.

## Rules
- Do not change the English wording, punctuation or capitalisation.
- Do not change logic, layout, property names or formatting of other code.
  Touch only the string expressions.
- Edit only the files in your list. Other agents are editing other folders at
  the same time.
- Do not run cargo, do not build, do not commit, do not create new files.
- If a line should NOT be wrapped (it is an identifier, a comparison, etc.),
  leave it and mention it in your report.
- When finished, print a short report: files changed, number of strings
  wrapped, and every line you deliberately skipped with a one-line reason.

## Your lines (file:line: current text; line numbers may shift as you edit)
