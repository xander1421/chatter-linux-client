#!/usr/bin/env bash
# Run on the destination host before installing the system-runtime package.
set -euo pipefail
command -v gst-inspect-1.0 >/dev/null || { echo 'FATAL: install GStreamer tools and runtime plugins' >&2; exit 1; }
for element in appsrc appsink queue decodebin decodebin3 uridecodebin playbin3; do gst-inspect-1.0 "$element" >/dev/null || exit 1; done
for element in audioconvert audioresample autoaudiosink videoconvert videoscale \
    qtdemux matroskademux oggdemux opusdec vorbisdec vp8dec vp9dec avdec_h264 avdec_aac avdec_gif; do gst-inspect-1.0 "$element" >/dev/null || exit 1; done
# Require an actual desktop audio backend, beyond autoaudiosink's factory.
gst-inspect-1.0 pulsesink >/dev/null 2>&1 || gst-inspect-1.0 pipewiresink >/dev/null
if [ "$#" -gt 0 ]; then
    command -v ldd >/dev/null || { echo 'FATAL: ldd is required' >&2; exit 1; }
    linked=$(ldd "$1") || exit 1
    if [[ "$linked" == *'not found'* ]]; then
        printf '%s\n' "$linked" >&2
        exit 1
    fi
fi
echo 'System library/plugin checks passed; session keyring, devices and live calls require separate verification.'
