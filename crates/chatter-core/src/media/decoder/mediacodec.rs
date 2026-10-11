//! Android byte-buffer video decoding through the API-21 NDK media API.
//!
//! Linear I420/NV12, BT.601/BT.709 limited/full-range output is supported; vendor/tiled,
//! HDR and other color layouts fail explicitly instead of guessing memory layout.
//! Audio playback is not yet implemented (the player never pulls decode_audio).

use super::{
    DecodedAudio, DecodedFrame, VideoDecoder, VideoInfo,
    yuv::{self, YuvLayout},
};
use crate::error::MediaError;
use std::{
    ffi::{CStr, c_char, c_void},
    fs::File,
    io::Write,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    ptr,
    time::{Duration, Instant},
};

type Handle = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct BufferInfo {
    offset: i32,
    size: i32,
    time_us: i64,
    flags: u32,
}

// Signatures mirror NDK r28 NdkMedia{Codec,Extractor,Format}.h. No Java VM,
// Activity or new dependency is needed. Every pointer is owned by Decoder/Format.
#[link(name = "mediandk")]
unsafe extern "C" {
    fn AMediaExtractor_new() -> Handle;
    fn AMediaExtractor_delete(ex: Handle) -> i32;
    fn AMediaExtractor_setDataSourceFd(ex: Handle, fd: i32, offset: i64, length: i64) -> i32;
    fn AMediaExtractor_getTrackCount(ex: Handle) -> usize;
    fn AMediaExtractor_getTrackFormat(ex: Handle, track: usize) -> Handle;
    fn AMediaExtractor_selectTrack(ex: Handle, track: usize) -> i32;
    fn AMediaExtractor_readSampleData(ex: Handle, data: *mut u8, capacity: usize) -> isize;
    fn AMediaExtractor_getSampleTime(ex: Handle) -> i64;
    fn AMediaExtractor_advance(ex: Handle) -> bool;
    fn AMediaExtractor_seekTo(ex: Handle, time_us: i64, mode: i32) -> i32;
    fn AMediaFormat_delete(format: Handle) -> i32;
    fn AMediaFormat_getInt32(format: Handle, key: *const c_char, value: *mut i32) -> bool;
    fn AMediaFormat_getInt64(format: Handle, key: *const c_char, value: *mut i64) -> bool;
    fn AMediaFormat_getString(
        format: Handle,
        key: *const c_char,
        value: *mut *const c_char,
    ) -> bool;
    fn AMediaFormat_setInt32(format: Handle, key: *const c_char, value: i32);
    fn AMediaCodec_createDecoderByType(mime: *const c_char) -> Handle;
    fn AMediaCodec_delete(codec: Handle) -> i32;
    fn AMediaCodec_configure(
        codec: Handle,
        format: Handle,
        surface: Handle,
        crypto: Handle,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_start(codec: Handle) -> i32;
    fn AMediaCodec_stop(codec: Handle) -> i32;
    fn AMediaCodec_flush(codec: Handle) -> i32;
    fn AMediaCodec_dequeueInputBuffer(codec: Handle, timeout_us: i64) -> isize;
    fn AMediaCodec_getInputBuffer(codec: Handle, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(
        codec: Handle,
        index: usize,
        offset: libc::off_t,
        size: usize,
        time_us: u64,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: Handle,
        info: *mut BufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputBuffer(codec: Handle, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_getOutputFormat(codec: Handle) -> Handle;
    fn AMediaCodec_releaseOutputBuffer(codec: Handle, index: usize, render: bool) -> i32;
}
fn error(message: impl Into<String>) -> MediaError {
    MediaError::PlaybackError(message.into())
}
fn status(code: i32) -> Result<(), MediaError> {
    if code == 0 {
        Ok(())
    } else {
        Err(error(format!("NDK media status {code}")))
    }
}
struct Format(Handle);
impl Format {
    fn int(&self, key: &CStr, default: i32) -> i32 {
        let mut value = default;
        unsafe {
            AMediaFormat_getInt32(self.0, key.as_ptr(), &mut value);
        }
        value
    }
}
impl Drop for Format {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                AMediaFormat_delete(self.0);
            }
        }
    }
}

pub struct MediaCodecDecoder {
    extractor: Handle,
    codec: Handle,
    started: bool,
    info: Option<VideoInfo>,
    file: Option<File>,
    input_eos: bool,
    output_eos: bool,
    position: u64,
    seek_target: u64,
    clock: Option<(Instant, u64)>,
    pending_frame: Option<DecodedFrame>,
}
// Handles have no thread affinity in synchronous NDK mode. Ownership may move
// between threads; all calls require &mut self and no buffer escapes a call.
unsafe impl Send for MediaCodecDecoder {}
impl MediaCodecDecoder {
    pub fn new() -> Result<Self, MediaError> {
        Ok(Self {
            extractor: ptr::null_mut(),
            codec: ptr::null_mut(),
            started: false,
            info: None,
            file: None,
            input_eos: false,
            output_eos: false,
            position: 0,
            seek_target: 0,
            clock: None,
            pending_frame: None,
        })
    }
    fn open_file(&mut self, file: File) -> Result<VideoInfo, MediaError> {
        self.close();
        let length = i64::try_from(file.metadata()?.len()).map_err(|_| error("video too large"))?;
        self.file = Some(file);
        // On any setup failure release all resources, including partially started codecs.
        let result = self.configure(length);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn configure(&mut self, length: i64) -> Result<VideoInfo, MediaError> {
        unsafe {
            self.extractor = AMediaExtractor_new();
            if self.extractor.is_null() {
                return Err(error("cannot create media extractor"));
            }
            status(AMediaExtractor_setDataSourceFd(
                self.extractor,
                self.file.as_ref().unwrap().as_raw_fd(),
                0,
                length,
            ))?;
            let mut selected = None;
            let mut has_audio = false;
            for track in 0..AMediaExtractor_getTrackCount(self.extractor) {
                let format = Format(AMediaExtractor_getTrackFormat(self.extractor, track));
                if format.0.is_null() {
                    continue;
                }
                let mut mime = ptr::null();
                if !AMediaFormat_getString(format.0, c"mime".as_ptr(), &mut mime) || mime.is_null()
                {
                    continue;
                }
                let name = CStr::from_ptr(mime).to_string_lossy();
                has_audio |= name.starts_with("audio/");
                if selected.is_none() && name.starts_with("video/") {
                    selected = Some((track, format, name.into_owned()));
                }
            }
            let (track, format, mime) = selected.ok_or_else(|| error("no video track"))?;
            let width = format.int(c"width", 0);
            let height = format.int(c"height", 0);
            if !(1..=8192).contains(&width)
                || !(1..=8192).contains(&height)
                || i64::from(width) * i64::from(height) > 4096 * 4096
            {
                return Err(error("invalid video dimensions"));
            }
            let mut duration = 0;
            AMediaFormat_getInt64(format.0, c"durationUs".as_ptr(), &mut duration);
            let info = VideoInfo {
                width: width as u32,
                height: height as u32,
                duration_ms: duration.max(0) as u64 / 1000,
                fps: format.int(c"frame-rate", 30).max(1) as f64,
                codec: mime,
                has_audio,
                is_seekable: true,
            };
            let mut mime_ptr = ptr::null();
            AMediaFormat_getString(format.0, c"mime".as_ptr(), &mut mime_ptr);
            self.codec = AMediaCodec_createDecoderByType(mime_ptr);
            if self.codec.is_null() {
                return Err(error("no Android decoder for video MIME"));
            }
            // Request a defined linear layout. The actual output format is still checked.
            AMediaFormat_setInt32(format.0, c"color-format".as_ptr(), 19);
            status(AMediaExtractor_selectTrack(self.extractor, track))?;
            status(AMediaCodec_configure(
                self.codec,
                format.0,
                ptr::null_mut(),
                ptr::null_mut(),
                0,
            ))?;
            status(AMediaCodec_start(self.codec))?;
            self.started = true;
            self.info = Some(info.clone());
            Ok(info)
        }
    }
    fn deliver_frame(&mut self, frame: DecodedFrame) -> Result<Option<DecodedFrame>, MediaError> {
        if let Some((start, position)) = self.clock {
            let due = Duration::from_millis(frame.timestamp_ms.saturating_sub(position));
            let wait = due.saturating_sub(start.elapsed());
            if wait > Duration::from_millis(50) {
                // Retain this frame and yield to the player's pause/seek/stop
                // checks. Never accelerate sparse/VFR video or sleep unbounded.
                self.pending_frame = Some(frame);
                std::thread::sleep(Duration::from_millis(50));
                return Err(error("no_sample: waiting for Android presentation time"));
            }
            std::thread::sleep(wait);
        }
        self.position = frame.timestamp_ms;
        Ok(Some(frame))
    }

    fn frame(&self, info: &BufferInfo, index: usize) -> Result<DecodedFrame, MediaError> {
        unsafe {
            let format = Format(AMediaCodec_getOutputFormat(self.codec));
            if format.0.is_null() {
                return Err(error("missing output format"));
            }
            let color = format.int(c"color-format", 0);
            if color != 19 && color != 21 {
                return Err(error(format!("unsupported Android YUV layout {color}")));
            }
            let standard = format.int(c"color-standard", 0);
            let range = format.int(c"color-range", 0);
            if ![0, 1, 2, 4].contains(&standard) || ![0, 1, 2].contains(&range) {
                return Err(error("unsupported video color standard/range"));
            }
            let width = format.int(c"width", 0);
            let height = format.int(c"height", 0);
            if !(1..=8192).contains(&width)
                || !(1..=8192).contains(&height)
                || i64::from(width) * i64::from(height) > 4096 * 4096
            {
                return Err(error("invalid Android output dimensions"));
            }
            let left = format.int(c"crop-left", 0);
            let top = format.int(c"crop-top", 0);
            let right = format.int(c"crop-right", width - 1);
            let bottom = format.int(c"crop-bottom", height - 1);
            let stride = format.int(c"stride", width);
            let slice_height = format.int(c"slice-height", height);
            if left < 0
                || top < 0
                || right < left
                || bottom < top
                || right >= width
                || bottom >= height
            {
                return Err(error("invalid Android video crop"));
            }
            let mut capacity = 0;
            let data = AMediaCodec_getOutputBuffer(self.codec, index, &mut capacity);
            let offset =
                usize::try_from(info.offset).map_err(|_| error("negative buffer offset"))?;
            let size = usize::try_from(info.size).map_err(|_| error("negative buffer size"))?;
            if data.is_null() || offset.checked_add(size).is_none_or(|end| end > capacity) {
                return Err(error("invalid Android output buffer"));
            }
            let width = (right - left + 1) as usize;
            let height = (bottom - top + 1) as usize;
            let pixels = yuv::to_rgb(
                std::slice::from_raw_parts(data.add(offset), size),
                YuvLayout {
                    width,
                    height,
                    stride: stride as usize,
                    slice_height: slice_height as usize,
                    left: left as usize,
                    top: top as usize,
                    planar: color == 19,
                    bt709: standard == 1,
                    full_range: range == 1,
                },
            )
            .map_err(error)?;
            Ok(DecodedFrame {
                data: pixels,
                width: width as u32,
                height: height as u32,
                timestamp_ms: info.time_us.max(0) as u64 / 1000,
            })
        }
    }
}
impl VideoDecoder for MediaCodecDecoder {
    fn open(&mut self, path: &Path) -> Result<VideoInfo, MediaError> {
        self.open_file(File::open(path)?)
    }
    fn open_memory(&mut self, data: Vec<u8>) -> Result<VideoInfo, MediaError> {
        // Linux anonymous memory fd works without HOME, /tmp or an Activity.
        // Use the kernel syscall: the libc wrapper only exists from API 30,
        // while Chatter supports API 26. Kernels since 3.17 support memfd.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"chatter-video".as_ptr(),
                libc::MFD_CLOEXEC,
            ) as i32
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&data)?;
        self.open_file(file)
    }
    fn decode_video_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        if !self.started {
            return Err(error("video is not open"));
        }
        if let Some(frame) = self.pending_frame.take() {
            return self.deliver_frame(frame);
        }
        if self.output_eos {
            return Ok(None);
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        loop {
            unsafe {
                if !self.input_eos {
                    let index = AMediaCodec_dequeueInputBuffer(self.codec, 0);
                    if index >= 0 {
                        let mut capacity = 0;
                        let buffer =
                            AMediaCodec_getInputBuffer(self.codec, index as usize, &mut capacity);
                        if buffer.is_null() {
                            return Err(error("missing Android input buffer"));
                        }
                        let size = AMediaExtractor_readSampleData(self.extractor, buffer, capacity);
                        let time = AMediaExtractor_getSampleTime(self.extractor);
                        if size < 0 && time >= 0 {
                            return Err(error("Android extractor failed to read sample"));
                        }
                        self.input_eos = size < 0;
                        status(AMediaCodec_queueInputBuffer(
                            self.codec,
                            index as usize,
                            0,
                            size.max(0) as usize,
                            time.max(0) as u64,
                            if self.input_eos { 4 } else { 0 },
                        ))?;
                        if !self.input_eos {
                            AMediaExtractor_advance(self.extractor);
                        }
                    } else if index != -1 {
                        return Err(error(format!("Android input dequeue {index}")));
                    }
                }
                let mut info = BufferInfo::default();
                let index = AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, 10_000);
                if index >= 0 {
                    self.output_eos = info.flags & 4 != 0;
                    let result = if info.size > 0 && info.flags & 2 == 0 {
                        self.frame(&info, index as usize).map(Some)
                    } else {
                        Ok(None)
                    };
                    let released = status(AMediaCodec_releaseOutputBuffer(
                        self.codec,
                        index as usize,
                        false,
                    ));
                    let frame = result?;
                    released?;
                    if let Some(frame) = frame {
                        if frame.timestamp_ms < self.seek_target {
                            if self.output_eos {
                                return Ok(None);
                            }
                            continue;
                        }
                        return self.deliver_frame(frame);
                    }
                    if self.output_eos {
                        return Ok(None);
                    }
                } else if ![-1, -2, -3].contains(&index) {
                    return Err(error(format!("Android output dequeue {index}")));
                }
            }
            if Instant::now() >= deadline {
                return Err(error("no_sample: Android decoder timed out"));
            }
        }
    }
    fn decode_audio(&mut self) -> Result<Option<DecodedAudio>, MediaError> {
        if self.has_audio() {
            Err(error("Android audio decoding is not implemented"))
        } else {
            Ok(None)
        }
    }
    fn seek(&mut self, timestamp_ms: u64) -> Result<(), MediaError> {
        if !self.started {
            return Err(error("video is not open"));
        }
        let timestamp = timestamp_ms.min(self.duration_ms());
        let micros =
            i64::try_from(timestamp.saturating_mul(1000)).map_err(|_| error("seek overflow"))?;
        unsafe {
            status(AMediaExtractor_seekTo(self.extractor, micros, 0))?; // previous sync, then discard to target
            status(AMediaCodec_flush(self.codec))?;
        }
        self.input_eos = false;
        self.output_eos = false;
        // Before a frame is decoded, expose the sync point actually reached,
        // rather than claiming the requested (possibly non-keyframe) position.
        self.position =
            unsafe { AMediaExtractor_getSampleTime(self.extractor) }.max(0) as u64 / 1000;
        self.seek_target = timestamp;
        self.pending_frame = None;
        if self.clock.is_some() {
            self.clock = Some((Instant::now(), timestamp));
        }
        Ok(())
    }
    fn position_ms(&self) -> u64 {
        self.position
    }
    fn query_actual_position_ms(&self) -> Result<u64, MediaError> {
        if !self.started {
            return Err(error("video is not open"));
        }
        Ok(self.position)
    }
    fn duration_ms(&self) -> u64 {
        self.info.as_ref().map_or(0, |i| i.duration_ms)
    }
    fn has_audio(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.has_audio)
    }
    fn dimensions(&self) -> (u32, u32) {
        self.info.as_ref().map_or((0, 0), |i| (i.width, i.height))
    }
    fn fps(&self) -> f64 {
        self.info.as_ref().map_or(0., |i| i.fps)
    }
    fn is_seekable(&self) -> bool {
        self.started
    }
    fn play(&mut self) -> Result<(), MediaError> {
        if !self.started {
            return Err(error("video is not open"));
        }
        self.clock = Some((Instant::now(), self.position));
        Ok(())
    }
    fn pause(&mut self) -> Result<(), MediaError> {
        if !self.started {
            return Err(error("video is not open"));
        }
        self.clock = None;
        Ok(())
    }
    fn set_volume(&mut self, _volume: f64) -> Result<(), MediaError> {
        if self.has_audio() {
            Err(error("Android video audio playback is not implemented"))
        } else {
            Ok(())
        }
    }
    fn close(&mut self) {
        unsafe {
            if !self.codec.is_null() {
                if self.started {
                    AMediaCodec_stop(self.codec);
                }
                AMediaCodec_delete(self.codec);
            }
            if !self.extractor.is_null() {
                AMediaExtractor_delete(self.extractor);
            }
        }
        self.codec = ptr::null_mut();
        self.extractor = ptr::null_mut();
        self.started = false;
        self.info = None;
        self.file = None;
        self.input_eos = false;
        self.output_eos = false;
        self.position = 0;
        self.seek_target = 0;
        self.clock = None;
        self.pending_frame = None;
    }
}
impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        self.close();
    }
}
