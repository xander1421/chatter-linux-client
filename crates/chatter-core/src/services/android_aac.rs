//! Encode captured PCM as AAC in an MPEG-4 audio file using Android's codec.
//! The platform codec avoids bundling a second native encoder in the APK.

use std::ffi::{c_char, c_int, c_void};
use std::os::fd::AsRawFd;
use std::path::Path;

#[repr(C)]
#[derive(Default)]
struct BufferInfo {
    offset: i32,
    size: i32,
    presentation_time_us: i64,
    flags: u32,
}

#[link(name = "mediandk")]
unsafe extern "C" {
    fn AMediaFormat_new() -> *mut c_void;
    fn AMediaFormat_delete(format: *mut c_void) -> c_int;
    fn AMediaFormat_setString(format: *mut c_void, key: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(format: *mut c_void, key: *const c_char, value: i32);
    fn AMediaCodec_createEncoderByType(mime: *const c_char) -> *mut c_void;
    fn AMediaCodec_delete(codec: *mut c_void) -> c_int;
    fn AMediaCodec_configure(
        codec: *mut c_void,
        format: *const c_void,
        surface: *mut c_void,
        crypto: *mut c_void,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut c_void) -> c_int;
    fn AMediaCodec_stop(codec: *mut c_void) -> c_int;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut c_void, timeout_us: i64) -> isize;
    fn AMediaCodec_getInputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(
        codec: *mut c_void,
        index: usize,
        offset: usize,
        size: usize,
        time_us: u64,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut c_void,
        info: *mut BufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut c_void, index: usize, render: bool) -> c_int;
    fn AMediaCodec_getOutputFormat(codec: *mut c_void) -> *mut c_void;
    fn AMediaMuxer_new(fd: c_int, format: c_int) -> *mut c_void;
    fn AMediaMuxer_delete(muxer: *mut c_void) -> c_int;
    fn AMediaMuxer_addTrack(muxer: *mut c_void, format: *const c_void) -> isize;
    fn AMediaMuxer_start(muxer: *mut c_void) -> c_int;
    fn AMediaMuxer_writeSampleData(
        muxer: *mut c_void,
        track: usize,
        data: *const u8,
        info: *const BufferInfo,
    ) -> c_int;
    fn AMediaMuxer_stop(muxer: *mut c_void) -> c_int;
}

const MIME: &[u8] = b"audio/mp4a-latm\0";
const EOS: u32 = 4;
const CODEC_CONFIG: u32 = 2;

fn check(status: c_int, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed ({status})"))
    }
}

/// Encode interleaved 16-bit PCM. The file is ready for upload only after the
/// muxer has written its final index; failures leave the temp file unuploaded.
pub fn encode(path: &Path, pcm: &[i16], sample_rate: u32, channels: u16) -> Result<(), String> {
    let output = std::fs::File::create(path).map_err(|e| e.to_string())?;
    // SAFETY: every native handle is checked and destroyed before returning.
    unsafe {
        let format = AMediaFormat_new();
        let codec = AMediaCodec_createEncoderByType(MIME.as_ptr().cast());
        let muxer = AMediaMuxer_new(output.as_raw_fd(), 0); // MPEG-4
        if format.is_null() || codec.is_null() || muxer.is_null() {
            if !format.is_null() {
                AMediaFormat_delete(format);
            }
            if !codec.is_null() {
                AMediaCodec_delete(codec);
            }
            if !muxer.is_null() {
                AMediaMuxer_delete(muxer);
            }
            return Err("Android AAC encoder unavailable".into());
        }
        AMediaFormat_setString(format, b"mime\0".as_ptr().cast(), MIME.as_ptr().cast());
        AMediaFormat_setInt32(format, b"sample-rate\0".as_ptr().cast(), sample_rate as i32);
        AMediaFormat_setInt32(format, b"channel-count\0".as_ptr().cast(), channels as i32);
        AMediaFormat_setInt32(format, b"bitrate\0".as_ptr().cast(), 48_000);
        AMediaFormat_setInt32(format, b"aac-profile\0".as_ptr().cast(), 2); // AAC LC
        let result = (|| {
            check(
                AMediaCodec_configure(codec, format, std::ptr::null_mut(), std::ptr::null_mut(), 1),
                "configure AAC",
            )?;
            check(AMediaCodec_start(codec), "start AAC")?;
            let mut cursor = 0usize;
            let mut input_done = false;
            let mut muxer_started = false;
            let mut track = 0usize;
            let mut idle = 0u32;
            loop {
                if !input_done {
                    let index = AMediaCodec_dequeueInputBuffer(codec, 10_000);
                    if index >= 0 {
                        let mut capacity = 0usize;
                        let buffer =
                            AMediaCodec_getInputBuffer(codec, index as usize, &mut capacity);
                        if buffer.is_null() || capacity < 2 * channels as usize {
                            return Err("AAC input buffer unavailable".into());
                        }
                        let available = pcm.len() - cursor;
                        let aligned = available / channels as usize * channels as usize;
                        let count =
                            (capacity / 2 / channels as usize * channels as usize).min(aligned);
                        // MediaCodec expects native-endian signed 16-bit PCM.
                        std::ptr::copy_nonoverlapping(
                            pcm[cursor..].as_ptr().cast::<u8>(),
                            buffer,
                            count * 2,
                        );
                        let presentation_us =
                            cursor as u64 / channels as u64 * 1_000_000 / sample_rate as u64;
                        cursor += count;
                        input_done = cursor + channels as usize > pcm.len();
                        check(
                            AMediaCodec_queueInputBuffer(
                                codec,
                                index as usize,
                                0,
                                count * 2,
                                presentation_us,
                                if input_done { EOS } else { 0 },
                            ),
                            "queue AAC input",
                        )?;
                    }
                }
                let mut info = BufferInfo::default();
                let index = AMediaCodec_dequeueOutputBuffer(codec, &mut info, 10_000);
                if index == -2 {
                    // output format changed
                    let out_format = AMediaCodec_getOutputFormat(codec);
                    if out_format.is_null() {
                        return Err("AAC output format unavailable".into());
                    }
                    let result = AMediaMuxer_addTrack(muxer, out_format);
                    AMediaFormat_delete(out_format);
                    if result < 0 {
                        return Err("add AAC track failed".into());
                    }
                    track = result as usize;
                    check(AMediaMuxer_start(muxer), "start AAC muxer")?;
                    muxer_started = true;
                } else if index >= 0 {
                    let mut capacity = 0usize;
                    let buffer = AMediaCodec_getOutputBuffer(codec, index as usize, &mut capacity);
                    if !muxer_started {
                        return Err("AAC muxer has no output format".into());
                    }
                    if info.size > 0 && info.flags & CODEC_CONFIG == 0 {
                        if buffer.is_null()
                            || info.offset < 0
                            || info.offset as usize + info.size as usize > capacity
                        {
                            return Err("AAC output buffer invalid".into());
                        }
                        check(
                            AMediaMuxer_writeSampleData(muxer, track, buffer, &info),
                            "write AAC sample",
                        )?;
                    }
                    check(
                        AMediaCodec_releaseOutputBuffer(codec, index as usize, false),
                        "release AAC output",
                    )?;
                    if info.flags & EOS != 0 {
                        break;
                    }
                } else {
                    idle += 1;
                    if idle > 3_000 {
                        return Err("AAC encoder timed out".into());
                    }
                }
                if index >= 0 {
                    idle = 0;
                }
            }
            if muxer_started {
                check(AMediaMuxer_stop(muxer), "finish AAC muxer")?;
            }
            Ok(())
        })();
        AMediaCodec_stop(codec);
        AMediaCodec_delete(codec);
        AMediaFormat_delete(format);
        AMediaMuxer_delete(muxer);
        result
    }
}
