"""
Utilities for stripping test code from Rust source files before line-counting.
Removes both dedicated test files and inline `#[cfg(test)]` modules/items.
"""

import re
from pathlib import Path


def is_test_file(path: Path) -> bool:
    """Returns True if the file or folder is dedicated to testing."""
    name_lower = path.name.lower()
    return "test" in name_lower or "testing.rs" in name_lower


def strip_cfg_test(code: str) -> str:
    """
    Strips items annotated with #[cfg(test)] or #[cfg(all(test, ...))].
    Correctly ignores braces inside strings, raw strings, chars, and comments.
    """
    i = 0
    n = len(code)
    out = []

    while i < n:
        # Line comments
        if code.startswith("//", i):
            end = code.find("\n", i)
            if end == -1:
                end = n
            out.append(code[i:end])
            i = end
            continue

        # Block comments
        if code.startswith("/*", i):
            end = code.find("*/", i)
            if end == -1:
                end = n
            else:
                end += 2
            out.append(code[i:end])
            i = end
            continue

        # Raw string literal: r"..." or r#"..."#
        if code[i] == "r" and i + 1 < n and (code[i + 1] == '"' or code[i + 1] == "#"):
            hashes = 0
            idx = i + 1
            while idx < n and code[idx] == "#":
                hashes += 1
                idx += 1
            if idx < n and code[idx] == '"':
                end_pat = '"' + "#" * hashes
                end = code.find(end_pat, idx + 1)
                if end == -1:
                    end = n
                else:
                    end += len(end_pat)
                out.append(code[i:end])
                i = end
                continue

        # Standard string literal: "..."
        if code[i] == '"':
            idx = i + 1
            while idx < n:
                if code[idx] == "\\":
                    idx += 2
                elif code[idx] == '"':
                    idx += 1
                    break
                else:
                    idx += 1
            out.append(code[i:idx])
            i = idx
            continue

        # Char literal: '.'
        if code[i] == "'" and i + 1 < n:
            idx = i + 1
            if idx < n and code[idx] == "\\":
                idx += 2
            else:
                idx += 1
            if idx < n and code[idx] == "'":
                idx += 1
                out.append(code[i:idx])
                i = idx
                continue

        # Attribute starting with #[cfg(
        if code.startswith("#[cfg(", i):
            attr_end = code.find("]", i)
            if attr_end != -1:
                attr = code[i : attr_end + 1]
                # Only `cfg(test)` and `cfg(all(test, ..))` require test, not `any(test, ..)`.
                test_only = re.fullmatch(r"#\[cfg\(\s*(test|all\(.*\btest\b.*\))\s*\)\]", attr)
                if test_only and not re.search(r"not\(\s*test\s*\)", attr):
                    i = attr_end + 1
                    # Skip whitespace, comments, and consecutive attributes
                    while i < n:
                        if code[i].isspace():
                            i += 1
                        elif code.startswith("//", i):
                            end = code.find("\n", i)
                            i = n if end == -1 else end
                        elif code.startswith("/*", i):
                            end = code.find("*/", i)
                            i = n if end == -1 else end + 2
                        elif code.startswith("#[", i):
                            end = code.find("]", i)
                            i = n if end == -1 else end + 1
                        else:
                            break

                    # Skip the item: up to ';' or a balanced '{ ... }', or up to ',' for a
                    # field or argument; stop before the ')', ']' or '}' that encloses it.
                    brace_depth = 0
                    nesting = 0
                    while i < n:
                        c = code[i]
                        if c == '"':
                            end = code.find('"', i + 1)
                            i = n if end == -1 else end + 1
                            continue
                        if c in "([":
                            nesting += 1
                        elif c in ")]":
                            if nesting == 0:
                                break
                            nesting -= 1
                        elif c == "}" and nesting == 0:
                            break
                        if c == ";" or (c == "," and nesting == 0):
                            i += 1
                            break
                        if c == "{":
                            brace_depth = 1
                            i += 1
                            while i < n and brace_depth > 0:
                                ch = code[i]
                                if code.startswith("//", i):
                                    end = code.find("\n", i)
                                    i = n if end == -1 else end
                                elif code.startswith("/*", i):
                                    end = code.find("*/", i)
                                    i = n if end == -1 else end + 2
                                elif ch == '"':
                                    end = code.find('"', i + 1)
                                    i = n if end == -1 else end + 1
                                elif ch == "{":
                                    brace_depth += 1
                                    i += 1
                                elif ch == "}":
                                    brace_depth -= 1
                                    i += 1
                                else:
                                    i += 1
                            break
                        i += 1
                    continue

        out.append(code[i])
        i += 1

    return "".join(out)


def clean_rust_sources(source_paths: list[Path], target_dir: Path) -> int:
    """
    Finds all non-test .rs files in source_paths, strips #[cfg(test)] blocks,
    and writes them into target_dir. Returns the number of files written.
    """
    written = 0
    for p in source_paths:
        if p.is_file() and p.suffix == ".rs":
            files = [p]
        else:
            files = list(p.rglob("*.rs"))

        for f in files:
            # Test files, and every file under a test folder (e.g. `src/parallel/tests/`).
            parts = [f] if p.is_file() else [p / q for q in f.relative_to(p).parts]
            if any(is_test_file(q) for q in parts):
                continue
            content = f.read_text(encoding="utf-8", errors="ignore")
            cleaned = strip_cfg_test(content)
            out_file = target_dir / f"mod_{written}_{f.name}"
            out_file.write_text(cleaned, encoding="utf-8")
            written += 1

    return written
