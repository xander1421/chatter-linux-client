#!/usr/bin/env bash
set -euo pipefail
umask 077
root=$(cd "$(dirname "$0")" && pwd)
stdlib=${1:-}
if [[ ! -f "$stdlib" ]]; then
    printf 'Usage: %s /path/to/kotlin-stdlib.jar\n' "$0" >&2
    exit 2
fi
production="$root/../app/build/tmp/kotlin-classes/debug"
if [[ ! -d "$production" ]]; then
    printf 'Compile the Android debug Kotlin classes before running this harness.\n' >&2
    exit 2
fi
classes=$(mktemp -d)
trap 'rm -rf -- "$classes"' EXIT
javac -cp "$production:$stdlib" -d "$classes" "$root"/android/os/Build.java "$root"/android/graphics/Bitmap.java "$root"/android/media/MediaDataSource.java "$root"/android/media/MediaMetadataRetriever.java "$root"/VideoThumbnailTest.java
java -cp "$classes:$production:$stdlib" VideoThumbnailTest
