#!/usr/bin/env bash
set -euo pipefail
umask 077
root=$(cd "$(dirname "$0")" && pwd)
classes=$(mktemp -d /home/alex/.cache/chatter-rc81/video-kotlin-fixture.XXXXXX)
stdlib=$(rg --files /home/alex/.gradle/caches/modules-2/files-2.1/org.jetbrains.kotlin/kotlin-stdlib/2.1.0 | rg '\.jar$' | head -1)
production="$root/../app/build/tmp/kotlin-classes/debug"
javac -cp "$production:$stdlib" -d "$classes" "$root"/android/os/Build.java "$root"/android/graphics/Bitmap.java "$root"/android/media/MediaDataSource.java "$root"/android/media/MediaMetadataRetriever.java "$root"/VideoThumbnailTest.java
java -cp "$classes:$production:$stdlib" VideoThumbnailTest
