#!/usr/bin/env python3
"""Bundle locked Cargo dependency licenses for offline release notices."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tomllib
from collections import defaultdict


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "assets/licenses/THIRD_PARTY_NOTICES.txt"


def vendor_packages():
    """Include vendor trees even when Cargo metadata does not select a patch."""
    for manifest in sorted((ROOT / "vendor").glob("*/Cargo.toml")):
        with manifest.open("rb") as source:
            package = tomllib.load(source).get("package", {})
        if "name" not in package or "version" not in package:
            raise ValueError(f"Missing package name/version: {manifest}")
        yield {
            "name": package["name"],
            "version": package["version"],
            "license": package.get("license"),
            "repository": package.get("repository"),
            "manifest_path": str(manifest),
        }


def notice_files(directory):
    for path in sorted(directory.iterdir(), key=lambda path: path.name.casefold()):
        if path.is_file() and path.name.upper().startswith(("LICENSE", "COPYING", "NOTICE")):
            yield path


def package_key(package):
    return (package["name"], package["version"], package["manifest_path"])


def main():
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    )
    metadata = json.loads(result.stdout)
    workspace_members = set(metadata["workspace_members"])
    packages = {
        package_key(package): package
        for package in metadata["packages"]
        if package["id"] not in workspace_members
    }
    for package in vendor_packages():
        packages.setdefault(package_key(package), package)

    grouped = defaultdict(list)
    texts = defaultdict(lambda: {"content": "", "crates": set()})
    notices = []
    for package in sorted(packages.values(), key=package_key):
        label = f'{package["name"]} {package["version"]}'
        expression = package.get("license") or "UNSPECIFIED"
        grouped[expression].append((label, package.get("repository") or "(not provided)"))
        directory = Path(package["manifest_path"]).parent
        for path in notice_files(directory):
            content = path.read_bytes()
            if path.name.upper().startswith("NOTICE"):
                notices.append((label, path.name, content.decode("utf-8")))
            else:
                digest = hashlib.sha256(content).hexdigest()
                texts[digest]["content"] = content.decode("utf-8")
                texts[digest]["crates"].add((label, path.name))

    lines = ["Chatter third-party notices", "Generated from locked Cargo metadata and vendor directories.", ""]
    for expression in sorted(grouped):
        lines.extend((f"License expression: {expression}", "=" * (20 + len(expression))))
        for label, repository in sorted(grouped[expression]):
            lines.append(f"  {label} — {repository}")
        lines.append("")

    lines.extend(("Full license texts (deduplicated by SHA-256)", "=" * 43, ""))
    for digest in sorted(texts):
        entry = texts[digest]
        lines.append(f"SHA-256: {digest}")
        lines.append("Used by:")
        for label, filename in sorted(entry["crates"]):
            lines.append(f"  {label} ({filename})")
        lines.extend(("", entry["content"], ""))

    lines.extend(("Per-crate NOTICE files", "=" * 22, ""))
    for label, filename, content in sorted(notices):
        lines.extend((f"{label} — {filename}", content, ""))

    output = "\n".join(lines) + "\n"
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(output, encoding="utf-8")
    print(f"{len(packages)} crates, {len(texts)} distinct license texts, {len(output.encode('utf-8'))} bytes")


if __name__ == "__main__":
    try:
        main()
    except (OSError, UnicodeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Cannot generate third-party notices: {error}", file=sys.stderr)
        sys.exit(1)
