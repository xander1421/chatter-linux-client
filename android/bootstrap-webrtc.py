#!/usr/bin/env python3
"""Fetch the exact pinned SDK's prebuilt Android archive, without compiling.

LK_CUSTOM_WEBRTC is exported by build-android.sh so Cargo and Java use this
same tree. Trust matches the SDK downloader: pinned GitHub release over HTTPS,
not an independently authenticated artifact checksum.
"""
import argparse
import fcntl
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import tempfile
import tomllib
import urllib.request
import zipfile

SDK_VERSION = "0.3.18"
TAG = "webrtc-51ef663"  # webrtc-sys-build 0.3.18 src/lib.rs WEBRTC_TAG
TRIPLE = "android-arm64-release"
MAX_ARCHIVE = 1024 * 1024 * 1024
MAX_EXPANDED = 3 * MAX_ARCHIVE


def validate_tree(root):
    for rel in ["libwebrtc.a", "libwebrtc.jar", "webrtc.ninja", "third_party/boringssl/src/include/openssl/ssl.h"]:
        if not (root / rel).is_file():
            raise ValueError("Incomplete pinned WebRTC prebuilt tree")


def extract_archive(archive, stage):
    with zipfile.ZipFile(archive) as bundle:
        total = 0
        for member in bundle.infolist():
            name = PurePosixPath(member.filename)
            mode = member.external_attr >> 16
            if (name.is_absolute() or ".." in name.parts or not name.parts
                    or name.parts[0] != TRIPLE or "\\" in member.filename
                    or stat.S_ISLNK(mode)):
                raise ValueError("Unsafe WebRTC archive entry")
            total += member.file_size
            if total > MAX_EXPANDED:
                raise ValueError("WebRTC archive exceeds expansion limit")
            destination = stage.joinpath(*name.parts)
            if member.is_dir():
                destination.mkdir(parents=True, exist_ok=True)
            else:
                destination.parent.mkdir(parents=True, exist_ok=True)
                with bundle.open(member) as source, destination.open("wb") as output:
                    shutil.copyfileobj(source, output)
                destination.chmod(0o644)
    validate_tree(stage / TRIPLE)


def bootstrap(project, target):
    lock = tomllib.loads((project / "Cargo.lock").read_text())
    versions = [p["version"] for p in lock["package"] if p["name"] == "webrtc-sys-build"]
    if versions != [SDK_VERSION]:
        raise ValueError("WebRTC SDK pin changed; review archive tag before bootstrapping")
    if os.environ.get("LK_DEBUG_WEBRTC") == "true":
        raise ValueError("Android SQLCipher bootstrap requires the release WebRTC archive")
    custom = os.environ.get("LK_CUSTOM_WEBRTC")
    if custom:
        root = Path(custom).resolve()
        validate_tree(root)
        return root
    parent = target / "livekit_webrtc" / TAG
    parent.mkdir(parents=True, exist_ok=True)
    root = parent / TRIPLE
    with (parent / ".bootstrap.lock").open("w") as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        if root.exists():
            validate_tree(root)
            return root
        with tempfile.TemporaryDirectory(prefix=".bootstrap-", dir=parent) as scratch:
            stage = Path(scratch)
            archive = stage / "webrtc.zip"
            url = f"https://github.com/livekit/rust-sdks/releases/download/{TAG}/webrtc-{TRIPLE}.zip"
            with urllib.request.urlopen(url, timeout=120) as response, archive.open("wb") as output:
                copied = 0
                while block := response.read(1024 * 1024):
                    copied += len(block)
                    if copied > MAX_ARCHIVE:
                        raise ValueError("WebRTC archive exceeds download limit")
                    output.write(block)
            extract_archive(archive, stage)
            (stage / TRIPLE).rename(root)
    return root


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("project", type=Path)
    parser.add_argument("target", type=Path)
    args = parser.parse_args()
    print(bootstrap(args.project.resolve(), args.target.resolve()))
