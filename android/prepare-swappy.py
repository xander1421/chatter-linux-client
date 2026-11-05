#!/usr/bin/env python3
"""Stage the pinned SDK for a debug-only, opt-in frame-pacing comparison."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import tempfile
import urllib.request
import zipfile

VERSION = "2.1.3"
SHA256 = "a9f83e0cf47d694ed2cea0ae995d5cf7411060d9d4b680610430d03d6b20ea27"
URL = f"https://dl.google.com/dl/android/maven2/androidx/games/games-frame-pacing/{VERSION}/games-frame-pacing-{VERSION}.aar"
ENTRY = "prefab/modules/swappy/libs/android.arm64-v8a/libswappy.so"
MARKER = ".chatter-swappy-runtime.sha256"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def disable(output):
    (output / "libswappy.so").unlink(missing_ok=True)
    marker = output / MARKER
    runtime = output / "libc++_shared.so"
    # Do not remove an unrelated runtime staged by another native dependency.
    if marker.exists() and runtime.exists() and marker.read_text().strip() == digest(runtime):
        runtime.unlink()
    marker.unlink(missing_ok=True)


def stage(archive, runtime, output):
    if digest(archive) != SHA256:
        raise ValueError("Swappy AAR checksum mismatch")
    with zipfile.ZipFile(archive) as sdk:
        library = sdk.read(ENTRY)
    if not runtime.is_file():
        raise ValueError("NDK libc++ shared runtime is missing")
    output.mkdir(parents=True, exist_ok=True)
    # The AAR and runtime are checked before modifying staged outputs.
    (output / "libswappy.so").write_bytes(library)
    target = output / "libc++_shared.so"
    if not target.exists():
        shutil.copyfile(runtime, target)
        (output / MARKER).write_text(digest(target) + "\n")


def acquire():
    supplied = os.environ.get("CHATTER_ANDROID_SWAPPY_AAR")
    if supplied:
        return Path(supplied)
    cache = Path.home() / ".cache/chatter-android-experiments"
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / f"games-frame-pacing-{VERSION}.aar"
    if archive.exists():
        return archive
    # Download into an unreferenced temporary file; publish only verified bytes.
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temp:
        pending = Path(temp.name)
        try:
            with urllib.request.urlopen(URL, timeout=60) as source:
                shutil.copyfileobj(source, temp)
            temp.flush()
            if digest(pending) != SHA256:
                raise ValueError("Downloaded Swappy AAR checksum mismatch")
            pending.replace(archive)
        finally:
            pending.unlink(missing_ok=True)
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--ndk", required=True, type=Path)
    args = parser.parse_args()
    if os.environ.get("CHATTER_ANDROID_SWAPPY_EXPERIMENT", "0") != "1":
        disable(args.output_dir)
        return
    if os.environ.get("RELEASE", "0") == "1":
        parser.error("Swappy experiment is debug-only")
    runtime = args.ndk / "toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so"
    stage(acquire(), runtime, args.output_dir)
    print(f">> debug-only Swappy {VERSION} staged (verified SHA256)")


if __name__ == "__main__":
    main()
