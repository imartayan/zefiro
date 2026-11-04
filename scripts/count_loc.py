#!/usr/bin/env python3
"""
Counts lines of Rust code (LOC) across zstd implementations using tokei.
Strips tests and #[cfg(test)] blocks via strip_tests.py to count production code.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from strip_tests import clean_rust_sources

REPOS = [
    {
        "name": "zefiro",
        "version": "1.0.0",
        "url": "https://github.com/imartayan/zefiro",
        "local_path": Path(__file__).resolve().parent.parent / "src",
        "src_paths": ["src"],
        "safe": "Some `unsafe` (checked with Miri)",
        "fuzzed": "✅",
        "parallel": "✅",
    },
    {
        "name": "rust-zstd",
        "version": "0.4.1",
        "url": "https://github.com/physwkim/rust-zstd",
        "crate_name": "rust-zstd-0.4.1",
        "src_paths": ["src"],
        "safe": "❌",
        "fuzzed": "❌",
        "parallel": "✅",
    },
    {
        "name": "ruzstd",
        "version": "0.9.1",
        "url": "https://github.com/KillingSpark/zstd-rs",
        "crate_name": "ruzstd-0.9.1",
        "src_paths": ["src"],
        "safe": "❌",
        "fuzzed": "⚠️",
        "parallel": "❌",
    },
    {
        "name": "zrip",
        "version": "0.8.16",
        "url": "https://github.com/paddor/zrip",
        "crate_name": "zrip-0.8.16",
        "extra_crates": ["zrip-core-0.11.1", "zrip-decode-0.8.13", "zrip-encode-0.9.4"],
        "src_paths": [
            "src",
            "crates/core/src",
            "crates/decode/src",
            "crates/encode/src",
            "crates/zrip-core/src",
            "crates/zrip-decode/src",
            "crates/zrip-encode/src",
        ],
        "safe": "✅",
        "fuzzed": "✅",
        "parallel": "❌",
    },
    {
        "name": "mzstd",
        "version": "0.2.11",
        "url": "https://github.com/M-Compressors/mzstd",
        "crate_name": "mzstd-0.2.11",
        "src_paths": ["src"],
        "safe": "✅",
        "fuzzed": "✅",
        "parallel": "❌",
    },
]

CACHE_DIR = Path(os.environ.get("CACHE_DIR", "/tmp/zstd_repos"))
CARGO_REGISTRY = Path.home() / ".cargo" / "registry" / "src"


def find_cargo_cache(crate_pattern: str) -> list[Path]:
    matches = []
    if CARGO_REGISTRY.exists():
        for reg in CARGO_REGISTRY.iterdir():
            if reg.is_dir():
                for p in reg.glob(crate_pattern):
                    if p.is_dir():
                        matches.append(p)
    return matches


def get_repo_dir(repo: dict) -> list[Path]:
    if "local_path" in repo and repo["local_path"].exists():
        return [repo["local_path"].parent]

    # The published crate of the stated version first; a clone of the default branch may differ.
    crate_name = repo.get("crate_name")
    if crate_name:
        all_found = find_cargo_cache(crate_name)
        for ec in repo.get("extra_crates", []):
            all_found.extend(find_cargo_cache(ec))
        if all_found:
            return all_found

    repo_dir = CACHE_DIR / repo["name"]
    if not repo_dir.exists():
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        try:
            subprocess.run(
                ["git", "clone", "--depth=1", repo["url"], str(repo_dir)],
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except Exception:
            pass

    return [repo_dir] if repo_dir.exists() else []


def count_loc(repo: dict) -> int:
    dirs = get_repo_dir(repo)
    if not dirs:
        return 0

    target_paths = []
    for d in dirs:
        matched = False
        for p in repo["src_paths"]:
            cand = d / p
            if cand.exists():
                target_paths.append(cand)
                matched = True
        if not matched:
            src_dir = d / "src"
            target_paths.append(src_dir if src_dir.exists() else d)

    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        clean_rust_sources(target_paths, tmp_path)

        cmd = ["tokei", "-t=Rust", "-o", "json", str(tmp_path)]
        try:
            res = subprocess.run(cmd, capture_output=True, text=True, check=True)
            data = json.loads(res.stdout)
            return data.get("Rust", {}).get("code", 0)
        except Exception as e:
            print(f"Error running tokei for {repo['name']}: {e}", file=sys.stderr)
            return 0


def format_loc(code: int) -> str:
    if code == 0:
        return "N/A"
    return f"~{max(1, round(code / 1000))}K"


def main():
    results = []
    for repo in REPOS:
        loc = count_loc(repo)
        rounded = format_loc(loc)
        results.append({**repo, "loc": loc, "rounded": rounded})

    print(f"{'Crate':<12} {'Version':<10} {'Exact LOC':<12} {'Rounded LOC':<12}")
    print("-" * 50)
    for r in results:
        print(f"{r['name']:<12} {r['version']:<10} {r['loc']:<12} {r['rounded']:<12}")

    print("\nMarkdown Table:\n")
    print("| Crate | Source LOC* | Safe | Fuzzed vs `libzstd` | Parallel Decompression |")
    print("| :--- | :---: | :---: | :---: | :---: |")
    for r in results:
        link = f"[{r['name']}]({r['url']}) ({r['version']})"
        print(f"| {link} | {r['rounded']} | {r['safe']} | {r['fuzzed']} | {r['parallel']} |")
    print("\n* Counted with tokei excluding tests and #[cfg(test)] blocks.  \n⚠️ ruzstd fuzzes against crashes and panics, but not differentially against libzstd.")


if __name__ == "__main__":
    main()
