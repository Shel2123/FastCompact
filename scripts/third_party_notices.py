#!/usr/bin/env python3
"""Writes the licenses of the crates linked into orbit for one target.

Usage: scripts/third_party_notices.py --target <triple> [--output <file>]

Follows the normal (not build or dev) dependencies of orbit-desktop that
apply to the target, skipping proc-macro crates, which run only at build
time. For each crate it writes the name, version, license expression and
the text of its LICENSE*, COPYING* and NOTICE* files.

Some crates ship no license files. Such a crate is listed under MIT when
that is one of its options, with the standard MIT text and the authors from
its Cargo.toml, or else under Apache-2.0, whose text is added once at the
end. A crate without a license expression, or without license files and
without one of those options, is an error.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY_PACKAGE = "orbit-desktop"
LICENSE_PREFIXES = ("license", "licence", "copying", "notice")
APACHE = "Apache-2.0"
MIT = "MIT"
MIT_TEXT = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""


def metadata(target):
    output = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
            "--manifest-path",
            str(ROOT / "Cargo.toml"),
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(output)


def is_proc_macro(package):
    return any("proc-macro" in target["kind"] for target in package["targets"])


def linked_packages(meta):
    """Ids of the packages linked into orbit-desktop for the target."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    packages = {p["id"]: p for p in meta["packages"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == BINARY_PACKAGE)
    seen = set()
    stack = [root]
    while stack:
        package = stack.pop()
        if package in seen or is_proc_macro(packages[package]):
            continue
        seen.add(package)
        for dep in nodes[package]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    return seen


def options(expression):
    """The license ids an SPDX expression names."""
    words = expression.replace("(", " ").replace(")", " ").split()
    return {word for word in words if word not in ("OR", "AND", "WITH")}


def license_files(package):
    directory = Path(package["manifest_path"]).parent
    files = []
    for path in sorted(directory.iterdir()):
        if path.is_file() and path.name.lower().startswith(LICENSE_PREFIXES):
            files.append(path)
    license_file = package.get("license_file")
    if license_file:
        path = (directory / license_file).resolve()
        if path.is_file() and path not in files:
            files.append(path)
    return files


def main():
    parser = argparse.ArgumentParser(
        description="Write the licenses of the crates linked into orbit."
    )
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", default="THIRD-PARTY-NOTICES.txt")
    args = parser.parse_args()

    meta = metadata(args.target)
    workspace = set(meta["workspace_members"])
    linked = linked_packages(meta)
    packages = sorted(
        (p for p in meta["packages"] if p["id"] in linked and p["id"] not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )

    errors = []
    sections = []
    apache_text = None
    needs_apache = False
    for package in packages:
        name = f'{package["name"]} {package["version"]}'
        expression = package.get("license")
        if not expression and not package.get("license_file"):
            errors.append(f"{name}: no license in Cargo.toml")
            continue
        files = license_files(package)
        lines = [
            "=" * 78,
            name,
            f'License: {expression or package.get("license_file")}',
        ]
        if package.get("repository"):
            lines.append(f'Source: {package["repository"]}')
        if not files:
            ids = options(expression or "")
            if MIT in ids:
                authors = ", ".join(package.get("authors") or []) or f'the {package["name"]} authors'
                print(f"warning: {name}: no license files, listed under {MIT}", file=sys.stderr)
                lines += ["", f"Used under {MIT}.", "", f"Copyright (c) {authors}", "", MIT_TEXT]
            elif APACHE in ids:
                print(f"warning: {name}: no license files, listed under {APACHE}", file=sys.stderr)
                needs_apache = True
                lines += ["", f"Used under {APACHE}; the license text is at the end of this file."]
            else:
                errors.append(f"{name}: no license files and no {MIT} or {APACHE} option")
                continue
        for path in files:
            text = path.read_text(errors="replace").rstrip()
            if apache_text is None and path.name.upper().startswith("LICENSE-APACHE"):
                apache_text = text
            lines += ["", f"--- {path.name} ---", text]
        sections.append("\n".join(lines))

    if needs_apache:
        if apache_text is None:
            errors.append(f"no {APACHE} text found among the license files")
        else:
            sections.append("\n".join(["=" * 78, f"{APACHE} license text", "", apache_text]))

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    header = (
        "Orbit includes the following third-party crates, built for "
        f"{args.target}.\n"
        "Their licenses and notices follow.\n"
    )
    Path(args.output).write_text(header + "\n" + "\n\n".join(sections) + "\n")
    print(f"{len(packages)} crates written to {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
