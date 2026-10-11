#!/usr/bin/env python3
"""Exercise the production NDK decoder on a host recording C ABI seam.

No Android runtime, device, services or real audio. This proves buffer/EOS/seek
and ownership handling; it does not prove any OEM codec's output layout.
Requires cc, ar, rustc and an already-built host libc rlib (or LIBC_RLIB).
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
workspace = root.parents[4]
libc = os.environ.get("LIBC_RLIB")
if libc is None:
    candidates = sorted((workspace / "target/debug/deps").glob("liblibc-*.rlib"),
                        key=lambda p: p.stat().st_mtime, reverse=True)
    if not candidates:
        raise SystemExit("Set LIBC_RLIB to a built host libc rlib")
    libc = str(candidates[0])

native = r'''
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
typedef struct { int sample; } Extractor;
typedef struct { int pending; int eos; uint64_t pts; unsigned char input[1024]; } Codec;
typedef struct { int32_t offset, size; int64_t pts; uint32_t flags; } Info;
static int codecs, extractors, formats, mode;
void fake_mode(int value) { mode=value; }
int fake_live_handles(void) { return codecs + extractors + formats; }
void *AMediaExtractor_new(void) { extractors++; return calloc(1,sizeof(Extractor)); }
int AMediaExtractor_delete(void *p) { extractors--; free(p); return 0; }
int AMediaExtractor_setDataSourceFd(void *p,int fd,int64_t off,int64_t len) {
    (void)p; (void)fd; (void)off; return len > 0 ? 0 : -1;
}
size_t AMediaExtractor_getTrackCount(void *p) { (void)p; return 1; }
static void *format_new(void) { formats++; return malloc(1); }
void *AMediaExtractor_getTrackFormat(void *p,size_t n) { (void)p; (void)n; return format_new(); }
int AMediaExtractor_selectTrack(void *p,size_t n) { (void)p; (void)n; return 0; }
ssize_t AMediaExtractor_readSampleData(void *p,unsigned char *buf,size_t cap) {
    if (((Extractor*)p)->sample) return -1;
    if (!cap) abort();
    buf[0]=1; return 1;
}
int64_t AMediaExtractor_getSampleTime(void *p) { return ((Extractor*)p)->sample ? -1 : 0; }
bool AMediaExtractor_advance(void *p) { ((Extractor*)p)->sample=1; return false; }
int AMediaExtractor_seekTo(void *p,int64_t ts,int mode) {
    (void)ts; (void)mode; ((Extractor*)p)->sample=0; return 0;
}
int AMediaFormat_delete(void *p) { formats--; free(p); return 0; }
bool AMediaFormat_getInt32(void *p,const char *key,int32_t *out) {
    (void)p;
    if (!strcmp(key,"width") || !strcmp(key,"height")) { *out=2; return true; }
    if (!strcmp(key,"color-format")) { *out=mode==1 ? 99 : 19; return true; }
    if (!strcmp(key,"frame-rate")) { *out=30; return true; }
    return false;
}
bool AMediaFormat_getInt64(void *p,const char *key,int64_t *out) {
    (void)p; (void)key; *out=1000000; return true;
}
bool AMediaFormat_getString(void *p,const char *key,const char **out) {
    (void)p; (void)key; *out="video/avc"; return true;
}
void AMediaFormat_setInt32(void *p,const char *key,int32_t value) { (void)p; (void)key; (void)value; }
void *AMediaCodec_createDecoderByType(const char *mime) { (void)mime; codecs++; return calloc(1,sizeof(Codec)); }
int AMediaCodec_delete(void *p) { codecs--; free(p); return 0; }
int AMediaCodec_configure(void *p,void *f,void *s,void *c,uint32_t flags) {
    (void)p; (void)f; (void)s; (void)c; (void)flags; return 0;
}
int AMediaCodec_start(void *p) { (void)p; return mode==3 ? -1 : 0; }
int AMediaCodec_stop(void *p) { (void)p; return 0; }
int AMediaCodec_flush(void *p) { memset(p,0,sizeof(Codec)); return 0; }
ssize_t AMediaCodec_dequeueInputBuffer(void *p,int64_t timeout) { (void)timeout; return ((Codec*)p)->pending ? -1 : 0; }
unsigned char *AMediaCodec_getInputBuffer(void *p,size_t n,size_t *out) { (void)n; *out=1024; return ((Codec*)p)->input; }
int AMediaCodec_queueInputBuffer(void *p,size_t n,off_t off,size_t size,uint64_t time,uint32_t flags) {
    (void)n; (void)off; (void)size; Codec *c=p; c->pending=1; c->eos=(flags&4)!=0; c->pts=time; return 0;
}
ssize_t AMediaCodec_dequeueOutputBuffer(void *p,Info *out,int64_t timeout) {
    (void)timeout; Codec *c=p; if (!c->pending) return -1;
    out->offset=mode==2 ? 99 : 0; out->size=c->eos ? 0 : 6; out->pts=c->pts; out->flags=c->eos ? 4 : 0; return 0;
}
unsigned char *AMediaCodec_getOutputBuffer(void *p,size_t n,size_t *out) {
    (void)p; (void)n; static unsigned char yuv[]={16,235,16,235,128,128}; *out=6; return yuv;
}
void *AMediaCodec_getOutputFormat(void *p) { (void)p; return format_new(); }
int AMediaCodec_releaseOutputBuffer(void *p,size_t n,bool render) { (void)n; (void)render; ((Codec*)p)->pending=0; return 0; }
'''
harness = r'''
#![allow(dead_code)]
mod error {
    #[derive(Debug)]
    pub enum MediaError { PlaybackError(String), InvalidFormat(String), FileRead(std::io::Error) }
    impl From<std::io::Error> for MediaError { fn from(e: std::io::Error) -> Self { Self::FileRead(e) } }
}
#[path=DECODER]
mod decoder;
use decoder::{DecodedAudio, DecodedFrame, VideoDecoder, VideoInfo};
#[path=YUV]
mod yuv;
#[path=MEDIACODEC]
mod mediacodec;
unsafe extern "C" { fn fake_live_handles() -> i32; fn fake_mode(value: i32); }
#[test]
fn actual_decoder_drains_eos_seeks_and_releases_all_handles() {
    fn assert_send<T: Send>() {}
    assert_send::<mediacodec::MediaCodecDecoder>();
    let mut dec = mediacodec::MediaCodecDecoder::new().unwrap();
    assert!(dec.decode_video_frame().is_err());
    assert!(dec.play().is_err());
    assert!(dec.pause().is_err());
    assert!(dec.query_actual_position_ms().is_err());
    assert!(dec.open_memory(vec![]).is_err());
    assert_eq!(unsafe { fake_live_handles() }, 0, "failed open leaked resources");
    unsafe { fake_mode(3); }
    assert!(dec.open_memory(vec![1]).is_err());
    assert_eq!(unsafe { fake_live_handles() }, 0, "failed codec start leaked resources");
    unsafe { fake_mode(0); }
    let info = dec.open_memory(vec![1]).unwrap();
    assert_eq!((info.width,info.height,info.duration_ms), (2,2,1000));
    assert!(!info.has_audio);
    dec.play().unwrap();
    let frame = dec.decode_video_frame().unwrap().unwrap();
    assert_eq!(frame.data, [0,0,0,255,255,255,0,0,0,255,255,255]);
    assert!(dec.decode_video_frame().unwrap().is_none());
    assert!(dec.decode_video_frame().unwrap().is_none());
    dec.pause().unwrap();
    dec.seek(0).unwrap();
    assert!(dec.decode_video_frame().unwrap().is_some(), "flush did not clear EOS");
    // Seeking past the last frame discards it and must reach EOS, not no_sample.
    dec.seek(500).unwrap();
    assert!(dec.decode_video_frame().unwrap().is_none());
    dec.close(); dec.close();
    assert_eq!(unsafe { fake_live_handles() }, 0, "close leaked resources");
    for mode in [1, 2] {
        dec.open_memory(vec![1]).unwrap();
        unsafe { fake_mode(mode); }
        assert!(dec.decode_video_frame().is_err(), "invalid output accepted");
        unsafe { fake_mode(0); }
        assert!(dec.decode_video_frame().unwrap().is_none(), "failed frame retained its output buffer");
    }
    dec.open_memory(vec![1]).unwrap();
    drop(dec);
    assert_eq!(unsafe { fake_live_handles() }, 0, "drop leaked resources");
}
'''.replace('DECODER', json.dumps(str(root / 'mod.rs'))).replace(
    'YUV', json.dumps(str(root / 'yuv.rs'))).replace(
    'MEDIACODEC', json.dumps(str(root / 'mediacodec.rs')))
with tempfile.TemporaryDirectory(prefix="chatter-ndk-decoder-") as temporary:
    directory = Path(temporary)
    (directory / "media.c").write_text(native)
    (directory / "decoder.rs").write_text(harness)
    subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", "-c", str(directory / "media.c"),
                    "-o", str(directory / "media.o")], check=True)
    subprocess.run(["ar", "rcs", str(directory / "libmediandk.a"), str(directory / "media.o")], check=True)
    subprocess.run(["rustc", "--edition=2024", "--test", str(directory / "decoder.rs"),
                    "--extern", f"libc={libc}", "-L", f"native={directory}",
                    "-L", f"dependency={Path(libc).parent}", "-o", str(directory / "check")], check=True)
    subprocess.run([str(directory / "check"), "--test-threads=1"], check=True)
