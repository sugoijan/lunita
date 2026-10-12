#!/usr/bin/env python3
"""strip_comments.py <src dir> <dest dir>: copies a tree of Rust sources, blanking its comments.

Every comment (`//`, `///`, `//!`, `/* */` with nesting) is replaced by nothing, keeping the
line structure: a line holding only a comment becomes empty, a comment after code is cut off,
each line of a block comment keeps its newline. Line numbers are unchanged, so a finding on the
copy points at the same line of the original. Comments holding `DELIBERATE(` are kept, with
the comment lines around them: they mark choices a reviewer should not report (see docs/hardening-notes.md). Strings, raw strings, byte
strings, char literals and lifetimes are recognised, so a `//` inside a string survives.
Files that are not `.rs` are copied as they are.
"""
import os
import shutil
import sys

KEEP = "DELIBERATE("


def kept_lines(src: str) -> set[int]:
    """The lines of `//` comments to keep: a run of consecutive comment-only lines is kept whole
    when one of its lines holds the marker, so a marked comment keeps its continuation lines."""
    kept, run = set(), []
    for number, line in enumerate(src.split("\n")):
        if line.lstrip().startswith("//"):
            run.append(number)
            continue
        if any(KEEP in src.split("\n")[k] for k in run):
            kept.update(run)
        run = []
    if any(KEEP in src.split("\n")[k] for k in run):
        kept.update(run)
    return kept


def strip(src: str) -> str:
    out = []
    i, n = 0, len(src)
    kept = kept_lines(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            j = n if j < 0 else j
            comment = src[i:j]
            line = src.count("\n", 0, i)
            out.append(comment if KEEP in comment or line in kept else "")
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src[j : j + 2] == "/*":
                    depth, j = depth + 1, j + 2
                elif src[j : j + 2] == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            comment = src[i:j]
            out.append(comment if KEEP in comment else "\n" * comment.count("\n"))
            i = j
        elif c == '"' or two in ('b"', 'c"'):
            j = i + (1 if c == '"' else 2)
            while src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            out.append(src[i : j + 1])
            i = j + 1
        elif c == "r" and src[i + 1 : i + 2] in ('"', "#") or two in ("br", "cr") and src[i + 2 : i + 3] in ('"', "#"):
            j = i + (1 if c == "r" else 2)
            hashes = 0
            while src[j] == "#":
                hashes, j = hashes + 1, j + 1
            if src[j] != '"':
                out.append(src[i:j])
                i = j
                continue
            close = '"' + "#" * hashes
            k = src.find(close, j + 1)
            k = n if k < 0 else k + len(close)
            out.append(src[i:k])
            i = k
        elif c == "'":
            # A char literal (`'x'`, `'\n'`, `'\u{1F600}'`, `b'x'`) or a lifetime (`'a`).
            if src[i + 1 : i + 2] == "\\":
                j = src.find("'", i + 2)
                out.append(src[i : j + 1])
                i = j + 1
            elif src[i + 2 : i + 3] == "'":
                out.append(src[i : i + 3])
                i += 3
            else:
                out.append(c)
                i += 1
        else:
            # Identifiers are copied whole, so `br` or `r` inside a name is not a raw string.
            if c.isalnum() or c == "_":
                j = i
                while j < n and (src[j].isalnum() or src[j] == "_"):
                    j += 1
                if src[j : j + 1] in ('"', "#") and src[i:j] in ("r", "br", "cr", "b", "c"):
                    continue_at = None
                else:
                    out.append(src[i:j])
                    i = j
                    continue
            out.append(c)
            i += 1
    text = "".join(out)
    # Trailing spaces left where a comment was cut off.
    return "\n".join(line.rstrip() for line in text.split("\n"))


def main() -> None:
    src_dir, dest_dir = sys.argv[1], sys.argv[2]
    files = 0
    for root, _, names in os.walk(src_dir):
        for name in names:
            path = os.path.join(root, name)
            dest = os.path.join(dest_dir, os.path.relpath(path, src_dir))
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            if name.endswith(".rs"):
                with open(path, encoding="utf-8") as f:
                    text = f.read()
                stripped = strip(text)
                assert stripped.count("\n") == text.count("\n"), path
                with open(dest, "w", encoding="utf-8") as f:
                    f.write(stripped)
                files += 1
            else:
                shutil.copyfile(path, dest)
    print(f"stripped {files} files into {dest_dir}")


if __name__ == "__main__":
    main()
