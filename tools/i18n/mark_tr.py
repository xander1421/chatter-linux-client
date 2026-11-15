#!/usr/bin/env python3
"""Mark plain UI string literals in .slint files with @tr(...).

Conservative first pass: only whole property statements on an allow-list of
display properties, with no string concatenation, no \\{...} interpolation and
no braces. Literals that are comparison operands (== / !=) are left alone.
Everything it will not touch is written to the TODO report for a reviewed
manual pass (format arguments, plurals).

  mark_tr.py [--write] [--todo FILE] ui/
"""
import re, sys, pathlib

PROPS = {
    "text", "accessible-label", "label", "placeholder", "title", "tooltip-text",
    "accessible-description", "subtitle", "detail", "hint", "cta-text",
    "accessible-title", "accessible-text", "action-label", "error-message",
    "placeholder-text", "empty-text", "caption", "description", "message",
    "confirm-label", "cancel-label", "header", "status-text", "button-text",
}
START = re.compile(r'^(\s*)([a-z][a-z0-9-]*)\s*:\s')
STR = re.compile(r'"((?:[^"\\]|\\.)*)"')
WORD = re.compile(r'[A-Za-z]{2,}')

def statements(lines):
    """Yield (first_line, last_line, prop) for allow-listed property statements."""
    i = 0
    while i < len(lines):
        m = START.match(lines[i])
        if m and m.group(2) in PROPS:
            j = i
            depth = 0
            while True:
                code = strip_comment(lines[j])
                for ch in STR.sub('""', code):
                    if ch in "([{": depth += 1
                    elif ch in ")]}": depth -= 1
                if (code.rstrip().endswith(";") and depth <= 0) or depth < 0 or j - i > 12 or j + 1 >= len(lines):
                    break
                j += 1
            yield i, j, m.group(2)
            i = j + 1
        else:
            i += 1

def strip_comment(line):
    out, in_str, k = [], False, 0
    while k < len(line):
        c = line[k]
        if in_str:
            out.append(c)
            if c == "\\" and k + 1 < len(line):
                out.append(line[k + 1]); k += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True; out.append(c)
        elif line.startswith("//", k):
            break
        else:
            out.append(c)
        k += 1
    return "".join(out)

def translatable(body):
    return bool(WORD.search(body.replace("\\n", " ")))

def process(path, write, todo):
    lines = path.read_text().split("\n")
    changed = 0
    for a, b, prop in statements(lines):
        block = "\n".join(lines[a:b + 1])
        code = "\n".join(strip_comment(l) for l in lines[a:b + 1])
        lits = [m for m in STR.finditer(code) if translatable(m.group(1))]
        if not lits or "@tr(" in code:
            continue
        nostr = STR.sub('""', code)
        hard = ('+' in nostr) or any('\\{' in m.group(1) or '{' in m.group(1) or '}' in m.group(1) for m in lits)
        if hard:
            todo.append(f"{path}:{a + 1}: {lines[a].strip()[:150]}")
            continue
        new_lines = []
        for l in lines[a:b + 1]:
            code_l = strip_comment(l)
            tail = l[len(code_l):]
            def repl(m, code_l=code_l):
                if not translatable(m.group(1)):
                    return m.group(0)
                before = code_l[:m.start()].rstrip()
                after = code_l[m.end():].lstrip()
                if before.endswith(("==", "!=")) or after.startswith(("==", "!=")):
                    return m.group(0)
                return f'@tr("{m.group(1)}")'
            new = STR.sub(repl, code_l)
            if new != code_l:
                changed += 1
            new_lines.append(new + tail)
        lines[a:b + 1] = new_lines
    if write and changed:
        path.write_text("\n".join(lines))
    return changed

def main():
    args = sys.argv[1:]
    write = "--write" in args
    todo_file = None
    if "--todo" in args:
        todo_file = args[args.index("--todo") + 1]
    roots = [a for a in args if not a.startswith("--") and a != todo_file]
    todo, total, files = [], 0, 0
    for root in roots:
        for path in sorted(pathlib.Path(root).rglob("*.slint")):
            n = process(path, write, todo)
            if n:
                files += 1; total += n
    print(f"marked {total} lines in {files} files; {len(todo)} statements need a manual pass")
    if todo_file:
        pathlib.Path(todo_file).write_text("\n".join(todo) + "\n")

if __name__ == "__main__":
    main()
