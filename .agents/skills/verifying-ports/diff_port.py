#!/usr/bin/env python3
"""Dump an upstream JS function and its Rust port side-by-side so
the porting claim has evidence behind it.

Usage:
    diff_port.py <js-file>:<js-fn> <rust-file>:<rust-fn>
    diff_port.py --markdown <js-file>:<js-fn> <rust-file>:<rust-fn>
    diff_port.py --rev <upstream-sha> <js-file>:<js-fn> <rust-file>:<rust-fn>

JS file is read from `upstream/master` (or `--rev`) via `git show`.
Rust file is read from the working tree. Both extractors do
brace-matching — they assume balanced `{}` and that the function
opens with `{` on the declaration line or the next line.

The script intentionally does NOT compute a "diff" — it dumps both
bodies + a small structural-stats footer, and leaves the actual
deviation list to the human writing the commit message. Mechanical
similarity is not a substitute for reading both.
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass


@dataclass
class FnExtract:
    file_path: str
    fn_name: str
    body: str
    start_line: int
    end_line: int


def read_upstream_file(path: str, rev: str) -> str:
    out = subprocess.run(
        ["git", "show", f"{rev}:{path}"],
        capture_output=True, text=True, check=False,
    )
    if out.returncode != 0:
        sys.exit(
            f"error: could not read {rev}:{path}\n{out.stderr.strip()}\n"
            f"hint: git fetch upstream && git rev-parse {rev}"
        )
    return out.stdout


def read_local_file(path: str) -> str:
    try:
        return open(path, encoding="utf-8").read()
    except FileNotFoundError:
        sys.exit(f"error: {path} not found")


def find_js_fn(src: str, fn: str) -> FnExtract:
    """Find a JS function by name. Handles `function fn(`, `fn = function(`,
    `const fn = (`, `async function fn(`, `fn: function(`. Picks the first
    match and brace-matches the body."""
    lines = src.split("\n")
    patterns = [
        re.compile(rf"^\s*(?:async\s+)?function\s+{re.escape(fn)}\s*\("),
        re.compile(rf"^\s*(?:const|let|var)\s+{re.escape(fn)}\s*=\s*(?:async\s*)?(?:function\s*\(|\([^)]*\)\s*=>)"),
        re.compile(rf"^\s*{re.escape(fn)}\s*[:=]\s*(?:async\s*)?(?:function\s*\(|\([^)]*\)\s*=>)"),
        # Method shorthand: `  fn(args) {` (inside a class/object)
        re.compile(rf"^\s*(?:async\s+)?{re.escape(fn)}\s*\([^)]*\)\s*\{{"),
    ]
    start_idx = None
    for i, line in enumerate(lines):
        if any(p.search(line) for p in patterns):
            start_idx = i
            break
    if start_idx is None:
        sys.exit(f"error: JS function `{fn}` not found")
    end_idx = brace_match_end(lines, start_idx)
    body = "\n".join(lines[start_idx : end_idx + 1])
    return FnExtract("", fn, body, start_idx + 1, end_idx + 1)


def find_rust_fn(src: str, fn: str) -> FnExtract:
    """Find a Rust fn by name. Handles `fn name(`, `pub fn name(`,
    `pub async fn name(`, `pub(crate) fn name(`."""
    lines = src.split("\n")
    pattern = re.compile(
        rf"^\s*(?:pub(?:\([^)]+\))?\s+)?(?:async\s+)?fn\s+{re.escape(fn)}\s*[<(]"
    )
    start_idx = None
    for i, line in enumerate(lines):
        if pattern.search(line):
            start_idx = i
            break
    if start_idx is None:
        sys.exit(f"error: Rust fn `{fn}` not found")
    end_idx = brace_match_end(lines, start_idx)
    body = "\n".join(lines[start_idx : end_idx + 1])
    return FnExtract("", fn, body, start_idx + 1, end_idx + 1)


def brace_match_end(lines: list[str], start: int) -> int:
    """Return the line index of the closing `}` that matches the first `{`
    at or after `start`. Ignores braces inside string literals and line
    comments — best-effort."""
    depth = 0
    started = False
    for i in range(start, len(lines)):
        line = strip_strings_and_comments(lines[i])
        for c in line:
            if c == "{":
                depth += 1
                started = True
            elif c == "}":
                depth -= 1
                if started and depth == 0:
                    return i
    sys.exit("error: unbalanced braces while extracting function body")


def strip_strings_and_comments(line: str) -> str:
    """Cheap heuristic: drop string contents and `// ...` comments so
    braces inside strings don't confuse the matcher. Doesn't handle
    block comments — fine for our codebases."""
    out = []
    in_str = None
    i = 0
    while i < len(line):
        c = line[i]
        if in_str:
            if c == "\\" and i + 1 < len(line):
                i += 2
                continue
            if c == in_str:
                in_str = None
                out.append("S")  # placeholder
            i += 1
            continue
        if c in "\"'`":
            in_str = c
            out.append("S")
            i += 1
            continue
        if c == "/" and i + 1 < len(line) and line[i + 1] == "/":
            break
        out.append(c)
        i += 1
    return "".join(out)


def structural_stats(body: str, lang: str) -> dict:
    """Quick mechanical fingerprint of a function: line count,
    conditional count, const/let/var or let/const count, function-call
    count. Not a similarity score — a sanity check the human reads."""
    lines = [l for l in body.split("\n") if l.strip()]
    cond_pat = re.compile(r"\b(if|else if|else|switch|while|for|match)\b")
    call_pat = re.compile(r"\b[a-zA-Z_][a-zA-Z_0-9]*\s*\(")
    if lang == "js":
        const_pat = re.compile(r"\b(const|let|var)\s+[a-zA-Z_]")
        ret_pat = re.compile(r"\breturn\b")
        throw_pat = re.compile(r"\bthrow\s")
    else:  # rust
        const_pat = re.compile(r"\blet\s+(?:mut\s+)?[a-zA-Z_]")
        ret_pat = re.compile(r"\breturn\b")
        throw_pat = re.compile(r"\?[\s;]|\bbail!\(|\breturn\s+Err\(")
    return {
        "lines (non-blank)": len(lines),
        "conditionals":      sum(len(cond_pat.findall(l)) for l in lines),
        "bindings":          sum(len(const_pat.findall(l)) for l in lines),
        "calls":             sum(len(call_pat.findall(l)) for l in lines),
        "early returns":     sum(len(ret_pat.findall(l)) for l in lines),
        "errors raised":     sum(len(throw_pat.findall(l)) for l in lines),
    }


def parse_target(s: str) -> tuple[str, str]:
    if ":" not in s:
        sys.exit(f"error: expected `file:function`, got `{s}`")
    f, fn = s.rsplit(":", 1)
    return f, fn


def emit(label: str, js: FnExtract, rs: FnExtract, markdown: bool) -> None:
    if markdown:
        print(f"### Upstream vs port — `{js.fn_name}` ↔ `{rs.fn_name}`\n")
        print("**Upstream (JS):**")
        print("```javascript")
        print(js.body)
        print("```\n")
        print("**Port (Rust):**")
        print("```rust")
        print(rs.body)
        print("```\n")
        print("**Structural stats:**")
        print()
        print("| metric | upstream | port |")
        print("|---|---|---|")
        a = structural_stats(js.body, "js")
        b = structural_stats(rs.body, "rust")
        for k in a:
            print(f"| {k} | {a[k]} | {b[k]} |")
        print()
    else:
        bar = "=" * 78
        print(bar)
        print(f"  Upstream JS — {label[0]}  (lines {js.start_line}-{js.end_line})")
        print(bar)
        print(js.body)
        print()
        print(bar)
        print(f"  Rust port — {label[1]}  (lines {rs.start_line}-{rs.end_line})")
        print(bar)
        print(rs.body)
        print()
        print(bar)
        print("  Structural stats")
        print(bar)
        a = structural_stats(js.body, "js")
        b = structural_stats(rs.body, "rust")
        widths = (20, 12, 12)
        print(f"  {'metric':<{widths[0]}} {'upstream':>{widths[1]}} {'port':>{widths[2]}}")
        for k in a:
            print(f"  {k:<{widths[0]}} {a[k]:>{widths[1]}} {b[k]:>{widths[2]}}")
        print()


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--rev", default="upstream/master",
                    help="upstream rev to read JS file from (default: upstream/master)")
    ap.add_argument("--markdown", action="store_true",
                    help="emit output formatted for pasting into a commit message")
    ap.add_argument("js_target", help="<js-file>:<js-fn>")
    ap.add_argument("rust_target", help="<rust-file>:<rust-fn>")
    args = ap.parse_args()

    js_file, js_fn = parse_target(args.js_target)
    rust_file, rust_fn = parse_target(args.rust_target)

    js_src = read_upstream_file(js_file, args.rev)
    rust_src = read_local_file(rust_file)

    js = find_js_fn(js_src, js_fn)
    rs = find_rust_fn(rust_src, rust_fn)

    emit((f"{js_file}:{js_fn}", f"{rust_file}:{rust_fn}"), js, rs, args.markdown)

    if not args.markdown:
        print("(write your DEVIATION LIST next — INTENTIONAL / ACCIDENTAL / UNVERIFIED)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
