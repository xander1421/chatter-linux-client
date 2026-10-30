//! Attachment media sanitization via Signal's `signal-media` (mp4san + webpsan).
//!
//! Two hooks, mirroring how Signal clients use these sanitizers:
//!
//! - **Outgoing** ([`sanitize_media`]): before upload/encryption. MP4 layout is
//!   normalized via mp4san (`moov` relocated before `mdat`) AND `moov.udta` /
//!   `moov.meta` are dropped by us (mp4san preserves them; they carry Apple
//!   GPS ©xyz and iTunes-style ItemList tags). JPEG APP1 (Exif+XMP — GPS,
//!   serials, timestamps) and COM, and PNG eXIf/tEXt/zTXt/iTXt/tIME are
//!   stripped losslessly (byte surgery, no re-encode). WebP structure is
//!   validated. Malformed files are rejected — we never emit bytes we could
//!   not parse.
//! - **Incoming** ([`validate_media`]): after decryption, before the plaintext is
//!   handed to renderers (image crate, FFmpeg). Malformed MP4/WebP is rejected so
//!   crafted files never reach the platform media parsers.
//!
//! Detection is content-based (magic bytes), not MIME-based, because our
//! renderers also sniff content — a lying MIME type must not bypass validation.
//!
//! coverage ceiling — sanitized: ISO-BMFF MP4 (isom brands), WebP
//! (validate), JPEG, PNG. Passes through byte-identical: GIF, MOV, HEIC, MKV,
//! PDF, …; non-`isom` ftyp is outside mp4san's scope and passes unchanged.
//! Filenames are NOT metadata we strip: `FileMetadata.filename` is E2E-
//! encrypted to the server and shown to the recipient by design (Signal
//! parity for document sends).

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use signal_media::sanitize::webp;

use super::AttachmentError;

// Bound each movie's sample tables and privacy metadata independently of media
// payload size. 16 MiB leaves room for hundreds of thousands of sample entries
// while keeping parser and privacy-copy memory practical on phones. This is a
// supported-metadata budget, not a duration/file-size or total RSS guarantee.
const MAX_MP4_METADATA_SIZE: u64 = 16 * 1024 * 1024;

fn mp4_config() -> mp4san::Config {
    mp4san::Config::builder()
        .max_metadata_size(MAX_MP4_METADATA_SIZE)
        .build()
}

fn check_mp4_metadata_size(size: u64) -> Result<(), AttachmentError> {
    if size > MAX_MP4_METADATA_SIZE {
        return Err(AttachmentError::InvalidAttachment(format!(
            "MP4 metadata exceeds the {} MiB upload budget; export with fewer metadata/sample-table entries",
            MAX_MP4_METADATA_SIZE / (1024 * 1024)
        )));
    }
    Ok(())
}

// mp4san can pad a relocated head to the old mdat offset. Its moov
// configuration does not bound this allocation, so preflight that span too.
const MAX_MP4_RELOCATED_HEAD_SIZE: u64 = 32 * 1024 * 1024;

fn preflight_mp4<R: Read + Seek>(reader: &mut R) -> Result<(), AttachmentError> {
    let io = |e| AttachmentError::FileError(format!("MP4 preflight io: {e}"));
    let bad = |m: &str| AttachmentError::InvalidAttachment(format!("malformed MP4 rejected: {m}"));
    let len = reader.seek(SeekFrom::End(0)).map_err(io)?;
    reader.seek(SeekFrom::Start(0)).map_err(io)?;
    // Preserve the non-isom carve-out. ftyp is separately capped at 1024
    // payload bytes by mp4san, before allocation. Inspect only that bounded
    // header here; let mp4san diagnose malformed/oversized ftyp itself.
    let mut first = [0u8; 1040];
    let n = read_up_to(reader, &mut first).map_err(io)?;
    if n < 8 || &first[4..8] != b"ftyp" {
        reader.seek(SeekFrom::Start(0)).map_err(io)?;
        return Ok(());
    }
    let size32 = u32::from_be_bytes(first[..4].try_into().unwrap());
    let (header, size) = if size32 == 1 && n >= 16 {
        (16, u64::from_be_bytes(first[8..16].try_into().unwrap()))
    } else {
        (8, if size32 == 0 { len } else { size32 as u64 })
    };
    if size > n as u64
        || size < (header + 8) as u64
        || !first[header + 8..size as usize]
            .chunks_exact(4)
            .any(|brand| brand == b"isom")
    {
        reader.seek(SeekFrom::Start(0)).map_err(io)?;
        return Ok(());
    }
    let mut offset = 0;
    let mut movie_offset = None;
    let mut media_offset = None;
    while offset < len {
        reader.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut bytes = [0; 16];
        let n = read_up_to(reader, &mut bytes).map_err(io)?;
        if n < 8 {
            return Err(bad("truncated box header"));
        }
        let size32 = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        let (header, size) = match size32 {
            0 => (8, len - offset),
            1 if n >= 16 => (16, u64::from_be_bytes(bytes[8..16].try_into().unwrap())),
            1 => return Err(bad("truncated largesize")),
            size => (8, size as u64),
        };
        match &bytes[4..8] {
            b"moov" => {
                check_mp4_metadata_size(size)?; // Before any metadata read/allocation.
                if movie_offset.replace(offset).is_some() {
                    return Err(bad("multiple moov boxes unsupported"));
                }
            }
            b"mdat" => {
                media_offset.get_or_insert(offset);
            }
            _ => {}
        }
        if size < header || size > len - offset {
            return Err(bad("box size out of bounds"));
        }
        offset += size;
    }
    if let (Some(movie), Some(media)) = (movie_offset, media_offset) {
        if movie > media && media > MAX_MP4_RELOCATED_HEAD_SIZE {
            return Err(AttachmentError::InvalidAttachment(
                "MP4 relocation padding exceeds the 32 MiB upload budget; export without large leading padding".into()
            ));
        }
    }
    reader.seek(SeekFrom::Start(0)).map_err(io)?;
    Ok(())
}

/// Result of sanitizing outgoing media bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum SanitizeOutcome {
    /// MP4 metadata was rewritten (relocated/stripped); use these bytes instead
    /// of the original.
    Rewritten(Vec<u8>),
    /// Bytes validated clean, or the format is not covered; use the original
    /// bytes unchanged.
    Unchanged,
}

fn looks_like_mp4(data: &[u8]) -> bool {
    data.len() >= 8 && &data[4..8] == b"ftyp"
}

fn looks_like_webp(data: &[u8]) -> bool {
    data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP"
}

fn looks_like_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data.starts_with(&[0xFF, 0xD8, 0xFF])
}

const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn looks_like_png(data: &[u8]) -> bool {
    data.len() >= 8 && data.starts_with(&PNG_SIG)
}

/// Strip privacy metadata from a JPEG without re-encoding: drop APP1
/// (Exif + XMP — the GPS carriers) and COM segments; keep everything else
/// (APP0/JFIF, APP2/ICC so colors stay correct, all tables/frames). From SOS
/// on, bytes are copied verbatim. Returns `None` when nothing was dropped.
///
/// Policy on malformed structure: reject — same as the MP4/WebP paths, we do
/// not send bytes we could not parse.
fn strip_jpeg_metadata(data: &[u8]) -> Result<Option<Vec<u8>>, AttachmentError> {
    let bad = |m: &str| AttachmentError::InvalidAttachment(format!("malformed JPEG rejected: {m}"));
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]); // SOI
    let mut i = 2;
    let mut dropped = false;
    loop {
        if i + 4 > data.len() {
            return Err(bad("truncated before SOS"));
        }
        if data[i] != 0xFF {
            return Err(bad("expected marker"));
        }
        let marker = data[i + 1];
        // Standalone markers between segments (rare, e.g. TEM/RSTn) — copy.
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            out.extend_from_slice(&data[i..i + 2]);
            i += 2;
            continue;
        }
        if marker == 0xDA {
            // SOS: entropy-coded data follows — copy the rest verbatim.
            out.extend_from_slice(&data[i..]);
            return Ok(if dropped { Some(out) } else { None });
        }
        if marker == 0xD9 {
            // EOI before SOS (metadata-only file) — copy and finish.
            out.extend_from_slice(&data[i..]);
            return Ok(if dropped { Some(out) } else { None });
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 || i + 2 + len > data.len() {
            return Err(bad("segment length out of bounds"));
        }
        let drop = marker == 0xE1 /* APP1: Exif + XMP */ || marker == 0xFE /* COM */;
        if drop {
            dropped = true;
        } else {
            out.extend_from_slice(&data[i..i + 2 + len]);
        }
        i += 2 + len;
    }
}

/// Strip privacy metadata from a PNG without re-encoding: drop eXIf (EXIF —
/// GPS carrier), tEXt/zTXt/iTXt (free text) and tIME chunks; keep all others.
/// Returns `None` when nothing was dropped.
fn strip_png_metadata(data: &[u8]) -> Result<Option<Vec<u8>>, AttachmentError> {
    let bad = |m: &str| AttachmentError::InvalidAttachment(format!("malformed PNG rejected: {m}"));
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&PNG_SIG);
    let mut i = 8;
    let mut dropped = false;
    while i < data.len() {
        if i + 8 > data.len() {
            return Err(bad("truncated chunk header"));
        }
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let ty = &data[i + 4..i + 8];
        let total = len
            .checked_add(12)
            .ok_or_else(|| bad("chunk length overflow"))?;
        if i + total > data.len() {
            return Err(bad("chunk length out of bounds"));
        }
        let drop = matches!(ty, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME");
        if drop {
            dropped = true;
        } else {
            out.extend_from_slice(&data[i..i + total]);
        }
        if ty == b"IEND" {
            break;
        }
        i += total;
    }
    Ok(if dropped { Some(out) } else { None })
}

// The file path uses fixed scratch storage even for a multi-GB IDAT or JPEG
// entropy tail. Do not size a buffer from an untrusted segment/chunk length.
const IMAGE_COPY_BUFFER_SIZE: usize = 8192;

fn image_io_error(e: std::io::Error) -> AttachmentError {
    AttachmentError::FileError(format!("sanitize image io: {e}"))
}

fn malformed_image(format: &str, message: &str) -> AttachmentError {
    AttachmentError::InvalidAttachment(format!("malformed {format} rejected: {message}"))
}

fn read_image_exact(
    input: &mut impl Read,
    buf: &mut [u8],
    format: &str,
) -> Result<(), AttachmentError> {
    input.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            malformed_image(format, "truncated input")
        } else {
            image_io_error(e)
        }
    })
}

/// Copy or discard an already bounds-checked span, still detecting a source
/// truncated after its length was obtained. Discarding reads (rather than seeks)
/// ensures missing metadata bytes/CRCs cannot silently pass validation.
fn transfer_image_span(
    input: &mut impl Read,
    output: &mut impl std::io::Write,
    mut len: u64,
    keep: bool,
    format: &str,
) -> Result<(), AttachmentError> {
    let mut scratch = [0u8; IMAGE_COPY_BUFFER_SIZE];
    while len != 0 {
        let count = len.min(scratch.len() as u64) as usize;
        read_image_exact(input, &mut scratch[..count], format)?;
        if keep {
            output
                .write_all(&scratch[..count])
                .map_err(image_io_error)?;
        }
        len -= count as u64;
    }
    Ok(())
}

/// File equivalent of `strip_jpeg_metadata`: preserve its marker policy,
/// including copying the entire tail verbatim from SOS (or an early EOI).
fn stream_jpeg_metadata(
    input: &mut impl Read,
    output: &mut impl std::io::Write,
    len: u64,
) -> Result<bool, AttachmentError> {
    let bad = |m| malformed_image("JPEG", m);
    let mut remaining = len.checked_sub(2).ok_or_else(|| bad("truncated SOI"))?;
    let mut soi = [0u8; 2];
    read_image_exact(input, &mut soi, "JPEG")?;
    output.write_all(&soi).map_err(image_io_error)?;
    let mut dropped = false;
    loop {
        // Match the bytes parser's four-byte lookahead, even for standalone
        // markers and SOS/EOI. Do not consume the next segment's bytes early.
        if remaining < 4 {
            return Err(bad("truncated before SOS"));
        }
        let mut marker = [0u8; 2];
        read_image_exact(input, &mut marker, "JPEG")?;
        remaining -= 2;
        if marker[0] != 0xFF {
            return Err(bad("expected marker"));
        }
        if marker[1] == 0x01 || (0xD0..=0xD7).contains(&marker[1]) {
            output.write_all(&marker).map_err(image_io_error)?;
            continue;
        }
        if marker[1] == 0xDA || marker[1] == 0xD9 {
            output.write_all(&marker).map_err(image_io_error)?;
            transfer_image_span(input, output, remaining, true, "JPEG")?;
            return Ok(dropped);
        }
        let mut size = [0u8; 2];
        read_image_exact(input, &mut size, "JPEG")?;
        let segment_len = u64::from(u16::from_be_bytes(size));
        if segment_len < 2 || segment_len > remaining {
            return Err(bad("segment length out of bounds"));
        }
        let drop = marker[1] == 0xE1 || marker[1] == 0xFE;
        if drop {
            dropped = true;
        } else {
            output.write_all(&marker).map_err(image_io_error)?;
            output.write_all(&size).map_err(image_io_error)?;
        }
        transfer_image_span(input, output, segment_len - 2, !drop, "JPEG")?;
        remaining -= segment_len;
    }
}

/// File equivalent of `strip_png_metadata`. Retained chunks (including their
/// original CRCs) are copied byte-exactly; CRC verification/PNG decoding is not
/// performed by the bytes parser and is not added here. Parsing ends at IEND.
fn stream_png_metadata(
    input: &mut impl Read,
    output: &mut impl std::io::Write,
    len: u64,
) -> Result<bool, AttachmentError> {
    let bad = |m| malformed_image("PNG", m);
    let mut remaining = len
        .checked_sub(8)
        .ok_or_else(|| bad("truncated signature"))?;
    let mut signature = [0u8; 8];
    read_image_exact(input, &mut signature, "PNG")?;
    output.write_all(&signature).map_err(image_io_error)?;
    let mut dropped = false;
    while remaining != 0 {
        if remaining < 8 {
            return Err(bad("truncated chunk header"));
        }
        let mut header = [0u8; 8];
        read_image_exact(input, &mut header, "PNG")?;
        // Widen before arithmetic: a u32 chunk length plus header/CRC must
        // neither wrap on 32-bit Android nor allocate a chunk-sized buffer.
        let chunk_len = u64::from(u32::from_be_bytes(header[..4].try_into().unwrap()));
        let total = chunk_len + 12;
        if total > remaining {
            return Err(bad("chunk length out of bounds"));
        }
        let ty = &header[4..];
        let drop = matches!(ty, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME");
        if drop {
            dropped = true;
        } else {
            output.write_all(&header).map_err(image_io_error)?;
        }
        transfer_image_span(input, output, chunk_len + 4, !drop, "PNG")?;
        if ty == b"IEND" {
            break;
        }
        remaining -= total;
    }
    Ok(dropped)
}

/// Own the partial output throughout parsing and flushing. All errors and the
/// unchanged case drop/unlink the temp; only a complete rewrite escapes.
fn sanitize_image_to_temp(
    input: &mut impl Read,
    len: u64,
    jpeg: bool,
    directory: &Path,
) -> Result<Option<tempfile::NamedTempFile>, AttachmentError> {
    use std::io::{BufWriter, Write};
    let mut temp = tempfile::NamedTempFile::new_in(directory).map_err(image_io_error)?;
    let dropped = {
        let mut output = BufWriter::with_capacity(IMAGE_COPY_BUFFER_SIZE, temp.as_file_mut());
        let dropped = if jpeg {
            stream_jpeg_metadata(input, &mut output, len)?
        } else {
            stream_png_metadata(input, &mut output, len)?
        };
        output.flush().map_err(image_io_error)?;
        dropped
    };
    Ok(if dropped { Some(temp) } else { None })
}

/// Sanitize outgoing media bytes before they leave the device.
///
/// Returns [`SanitizeOutcome::Rewritten`] when the MP4 metadata had to be
/// modified (caller must upload the rewritten bytes), [`SanitizeOutcome::Unchanged`]
/// when the input is already clean or is not a covered format.
///
/// # Errors
///
/// [`AttachmentError::InvalidAttachment`] if the input sniffs as MP4/WebP but
/// is malformed. Policy: reject — do not send bytes we could not parse.
///
/// Sync on purpose: signal-media's mp4 sanitizer is async, but its future is
/// not `Send` (holds `dyn ParsedBox` across awaits), which poisons any caller
/// spawned on tokio. Over an in-memory cursor it performs no real I/O, so
/// `block_on` never parks.
pub fn sanitize_media(content: &[u8]) -> Result<SanitizeOutcome, AttachmentError> {
    if looks_like_mp4(content) {
        return futures::executor::block_on(sanitize_mp4(content));
    }

    if looks_like_webp(content) {
        // webpsan validates only; it never rewrites.
        webp::sanitize(std::io::Cursor::new(content)).map_err(|e| {
            AttachmentError::InvalidAttachment(format!("malformed WebP rejected: {e}"))
        })?;
        return Ok(SanitizeOutcome::Unchanged);
    }

    if looks_like_jpeg(content) {
        return Ok(match strip_jpeg_metadata(content)? {
            Some(bytes) => SanitizeOutcome::Rewritten(bytes),
            None => SanitizeOutcome::Unchanged,
        });
    }

    if looks_like_png(content) {
        return Ok(match strip_png_metadata(content)? {
            Some(bytes) => SanitizeOutcome::Rewritten(bytes),
            None => SanitizeOutcome::Unchanged,
        });
    }

    Ok(SanitizeOutcome::Unchanged)
}

/// Validate incoming media bytes before handing them to renderers.
///
/// Same coverage and policy as [`sanitize_media`], but the result bytes are
/// discarded: an MP4 with trailing `moov` is valid and playable, it just is not
/// the layout we would emit ourselves.
///
pub fn validate_media(content: &[u8]) -> Result<(), AttachmentError> {
    sanitize_media(content).map(|_| ())
}

/// Sanitize a media file on disk without loading it into memory — for the
/// streaming (file-path) upload path, where attachments may be multi-GB.
///
/// Same coverage and policy as [`sanitize_media`]. Returns `Some(temp)` holding
/// the rewritten media when metadata had to be relocated/stripped (caller
/// encrypts the temp file instead of the original); `None` when the original
/// can be uploaded as-is.
///
/// Blocking file IO — call from `spawn_blocking`.
pub fn sanitize_media_file(
    path: &Path,
) -> Result<Option<tempfile::NamedTempFile>, AttachmentError> {
    let open_err =
        |e| AttachmentError::FileError(format!("Failed to open file for sanitizing: {e}"));
    let mut file = std::fs::File::open(path).map_err(open_err)?;

    let mut magic = [0u8; 12];
    let n = read_up_to(&mut file, &mut magic)
        .map_err(|e| AttachmentError::FileError(format!("Failed to sniff file: {e}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| AttachmentError::FileError(format!("Failed to rewind file: {e}")))?;

    if looks_like_webp(&magic[..n]) {
        // webpsan validates only; it never rewrites.
        webp::sanitize(BufReader::new(file)).map_err(|e| {
            AttachmentError::InvalidAttachment(format!("malformed WebP rejected: {e}"))
        })?;
        return Ok(None);
    }

    if looks_like_jpeg(&magic[..n]) || looks_like_png(&magic[..n]) {
        // Use the descriptor that was sniffed, not a second path lookup. Only
        // fixed-size input/output buffers are held while streaming to our temp.
        let len = file.metadata().map_err(image_io_error)?.len();
        super::ensure_attachments_dir().map_err(|e| {
            AttachmentError::FileError(format!("Failed to create attachments dir: {e}"))
        })?;
        return sanitize_image_to_temp(
            &mut BufReader::with_capacity(IMAGE_COPY_BUFFER_SIZE, file),
            len,
            looks_like_jpeg(&magic[..n]),
            &super::attachments_dir(),
        );
    }

    if !looks_like_mp4(&magic[..n]) {
        return Ok(None);
    }

    preflight_mp4(&mut file)?;

    // mp4san's sync API over the file directly (mediasan impls Skip for File).
    let sanitized = match mp4san::sanitize_with_config(BufReader::new(&mut file), mp4_config()) {
        Ok(s) => s,
        // Same carve-out as the bytes path: non-isom ftyp is outside scope.
        Err(mp4san::Error::Parse(report))
            if matches!(
                report.get_ref(),
                mp4san::parse::ParseError::UnsupportedFormat(_)
            ) =>
        {
            tracing::debug!("ftyp file outside mp4san scope, passing through");
            return Ok(None);
        }
        Err(e) => {
            return Err(AttachmentError::InvalidAttachment(format!(
                "MP4 rejected (metadata budget: 16 MiB; export a smaller metadata/sample table if exceeded): {e}"
            )));
        }
    };

    let Some(metadata) = sanitized.metadata else {
        // Layout already clean — but udta/meta GPS may still be present.
        return strip_mp4_file_privacy_boxes(&mut file);
    };
    // Strip privacy boxes from the rebuilt head (ftyp+moov) before writing.
    let metadata = match strip_mp4_privacy_boxes(&metadata)? {
        Some(stripped) => stripped,
        None => metadata,
    };

    super::ensure_attachments_dir().map_err(|e| {
        AttachmentError::FileError(format!("Failed to create attachments dir: {e}"))
    })?;
    let mut out = tempfile::NamedTempFile::new_in(super::attachments_dir())
        .map_err(|e| AttachmentError::FileError(format!("Failed to create sanitize temp: {e}")))?;

    use std::io::Write;
    out.write_all(&metadata).map_err(|e| {
        AttachmentError::FileError(format!("Failed to write sanitized metadata: {e}"))
    })?;
    file.seek(SeekFrom::Start(sanitized.data.offset))
        .map_err(|e| AttachmentError::FileError(format!("Failed to seek media data: {e}")))?;
    let copied = std::io::copy(&mut (&mut file).take(sanitized.data.len), out.as_file_mut())
        .map_err(|e| AttachmentError::FileError(format!("Failed to copy media data: {e}")))?;
    if copied != sanitized.data.len {
        return Err(AttachmentError::InvalidAttachment(format!(
            "MP4 data span truncated: expected {} bytes, copied {copied}",
            sanitized.data.len
        )));
    }
    out.flush()
        .map_err(|e| AttachmentError::FileError(format!("Failed to flush sanitized file: {e}")))?;

    Ok(Some(out))
}

/// File-streaming variant of [`strip_mp4_privacy_boxes`] for the case where
/// mp4san reports the layout already clean: walk top-level boxes via seeks,
/// load only budget-checked `moov` into memory, and if udta/meta must go,
/// stream-copy the file into a temp with the stripped moov spliced in.
fn strip_mp4_file_privacy_boxes(
    file: &mut std::fs::File,
) -> Result<Option<tempfile::NamedTempFile>, AttachmentError> {
    use std::io::Write;
    let bad = |m: &str| AttachmentError::InvalidAttachment(format!("malformed MP4 rejected: {m}"));
    let ferr = |e: std::io::Error| AttachmentError::FileError(format!("sanitize io: {e}"));

    let file_len = file.seek(SeekFrom::End(0)).map_err(ferr)?;
    file.seek(SeekFrom::Start(0)).map_err(ferr)?;

    // Pass 1: find moov, strip in memory.
    let mut moov_span = None; // One span only; never retain a table of input boxes.
    let mut stripped_moov: Option<Vec<u8>> = None;
    let mut off = 0u64;
    while off < file_len {
        let mut hdr = [0u8; 16];
        file.seek(SeekFrom::Start(off)).map_err(ferr)?;
        let n = read_up_to(file, &mut hdr).map_err(ferr)?;
        if n < 8 {
            return Err(bad("truncated box header"));
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let ty = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let size = match size32 {
            0 => file_len - off,
            1 => {
                if n < 16 {
                    return Err(bad("truncated largesize"));
                }
                u64::from_be_bytes(hdr[8..16].try_into().unwrap())
            }
            s => s,
        };
        if size < (if size32 == 1 { 16 } else { 8 }) || size > file_len - off {
            return Err(bad("box size out of bounds"));
        }
        let is_moov = &ty == b"moov";
        if is_moov {
            check_mp4_metadata_size(size)?;
            if moov_span.replace((off, size)).is_some() {
                return Err(bad("multiple moov boxes unsupported"));
            }
            let len = usize::try_from(size).map_err(|_| bad("moov too large"))?;
            let mut moov = vec![0u8; len];
            file.seek(SeekFrom::Start(off)).map_err(ferr)?;
            let got = read_up_to(file, &mut moov).map_err(ferr)?;
            if got != len {
                return Err(bad("moov truncated"));
            }
            stripped_moov = strip_mp4_privacy_boxes(&moov)?;
        }
        off += size;
    }

    let Some(new_moov) = stripped_moov else {
        return Ok(None);
    };

    // Pass 2: splice.
    super::ensure_attachments_dir().map_err(|e| {
        AttachmentError::FileError(format!("Failed to create attachments dir: {e}"))
    })?;
    let mut out = tempfile::NamedTempFile::new_in(super::attachments_dir())
        .map_err(|e| AttachmentError::FileError(format!("Failed to create sanitize temp: {e}")))?;
    let (moov_offset, moov_size) = moov_span.expect("stripped moov has a span");
    // Stream the prefix and suffix; replacing privacy boxes with free boxes
    // preserves the extent and every absolute chunk offset.
    for (offset, len) in [
        (0, moov_offset),
        (moov_offset + moov_size, file_len - moov_offset - moov_size),
    ] {
        if offset != 0 {
            out.write_all(&new_moov).map_err(ferr)?;
        }
        file.seek(SeekFrom::Start(offset)).map_err(ferr)?;
        let copied = std::io::copy(&mut file.take(len), out.as_file_mut()).map_err(ferr)?;
        if copied != len {
            return Err(bad("box copy truncated"));
        }
    }
    out.flush().map_err(ferr)?;
    Ok(Some(out))
}

/// `Read::read` may return short counts; loop until the buffer is full or EOF.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Erase privacy metadata children from a top-level `moov` box: `udta` (Apple
/// ©xyz GPS, iTunes ilst under udta.meta) and `meta`. mp4san normalizes
/// layout and validates structure but deliberately PRESERVES these boxes —
/// proven by the real-sample harness (exiftool-tagged GPSCoordinates
/// survived) — so we strip them ourselves. Presentation is unaffected: both
/// are non-presentation by definition. Replace them with equally sized,
/// zero-filled `free` boxes: shrinking moov would move mdat without updating
/// the absolute stco/co64 offsets already calculated by mp4san.
///
/// `data` is a whole MP4 (small enough to hold: callers pass either full
/// bytes or the mp4san metadata buffer, never a multi-GB mdat). Returns
/// `None` when nothing was dropped. moov with a 64-bit largesize is rejected
/// (a >4 GiB moov does not occur in real files).
fn strip_mp4_privacy_boxes(data: &[u8]) -> Result<Option<Vec<u8>>, AttachmentError> {
    let bad = |m: &str| AttachmentError::InvalidAttachment(format!("malformed MP4 rejected: {m}"));
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    let mut dropped = false;
    while i < data.len() {
        if i + 8 > data.len() {
            return Err(bad("truncated box header"));
        }
        let size32 = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as u64;
        let ty = &data[i + 4..i + 8];
        let (hdr, size) = match size32 {
            0 => (8u64, (data.len() - i) as u64), // box extends to EOF
            1 => {
                if i + 16 > data.len() {
                    return Err(bad("truncated largesize"));
                }
                let ls = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap());
                (16u64, ls)
            }
            s => (8u64, s),
        };
        if size < hdr || size > (data.len() - i) as u64 {
            return Err(bad("box size out of bounds"));
        }
        let end = i + size as usize;
        if ty == b"moov" {
            check_mp4_metadata_size(size)?;
            if hdr == 16 {
                return Err(bad("moov with 64-bit size unsupported"));
            }
            // Walk moov children, erasing udta/meta without moving sample data.
            let mut kept: Vec<u8> = Vec::with_capacity(size as usize);
            let mut j = i + 8;
            while j < end {
                if j + 8 > end {
                    return Err(bad("truncated moov child"));
                }
                let csize32 =
                    u32::from_be_bytes([data[j], data[j + 1], data[j + 2], data[j + 3]]) as u64;
                let cty = &data[j + 4..j + 8];
                let (chdr, csize) = match csize32 {
                    0 => (8u64, (end - j) as u64),
                    1 => {
                        if j + 16 > end {
                            return Err(bad("truncated moov child largesize"));
                        }
                        (
                            16u64,
                            u64::from_be_bytes(data[j + 8..j + 16].try_into().unwrap()),
                        )
                    }
                    s => (8u64, s),
                };
                if csize < chdr || csize > (end - j) as u64 {
                    return Err(bad("moov child size out of bounds"));
                }
                if cty == b"udta" || cty == b"meta" {
                    dropped = true;
                    let start = kept.len();
                    kept.extend_from_slice(&data[j..j + chdr as usize]);
                    kept[start + 4..start + 8].copy_from_slice(b"free");
                    kept.resize(start + csize as usize, 0);
                } else {
                    kept.extend_from_slice(&data[j..j + csize as usize]);
                }
                j += csize as usize;
            }
            let new_size = 8 + kept.len();
            out.extend_from_slice(
                &(u32::try_from(new_size).map_err(|_| bad("moov too large"))?).to_be_bytes(),
            );
            out.extend_from_slice(b"moov");
            out.extend_from_slice(&kept);
        } else {
            out.extend_from_slice(&data[i..end]);
        }
        i = end;
    }
    Ok(if dropped { Some(out) } else { None })
}

async fn sanitize_mp4(content: &[u8]) -> Result<SanitizeOutcome, AttachmentError> {
    preflight_mp4(&mut std::io::Cursor::new(content))?;
    match mp4san::sanitize_async_with_config(futures::io::Cursor::new(content), mp4_config()).await
    {
        Ok(sanitized) => match sanitized.metadata {
            Some(metadata) => {
                let start = usize::try_from(sanitized.data.offset).map_err(|_| {
                    AttachmentError::InvalidAttachment("MP4 data offset overflow".into())
                })?;
                let len = usize::try_from(sanitized.data.len).map_err(|_| {
                    AttachmentError::InvalidAttachment("MP4 data length overflow".into())
                })?;
                let end = start
                    .checked_add(len)
                    .filter(|&e| e <= content.len())
                    .ok_or_else(|| {
                        AttachmentError::InvalidAttachment("MP4 data span out of bounds".into())
                    })?;

                let mut output = Vec::with_capacity(metadata.len() + len);
                output.extend_from_slice(&metadata);
                output.extend_from_slice(&content[start..end]);
                let output = match strip_mp4_privacy_boxes(&output)? {
                    Some(stripped) => stripped,
                    None => output,
                };
                Ok(SanitizeOutcome::Rewritten(output))
            }
            // Layout already clean — but udta/meta GPS may still be present.
            None => Ok(match strip_mp4_privacy_boxes(content)? {
                Some(stripped) => SanitizeOutcome::Rewritten(stripped),
                None => SanitizeOutcome::Unchanged,
            }),
        },
        // Not an isom-brand MP4 (HEIC photo, QuickTime .mov, …): outside
        // mp4san's supported scope — treated as a non-covered format.
        Err(mp4san::Error::Parse(report))
            if matches!(
                report.get_ref(),
                mp4san::parse::ParseError::UnsupportedFormat(_)
            ) =>
        {
            tracing::debug!("ftyp file outside mp4san scope, passing through");
            Ok(SanitizeOutcome::Unchanged)
        }
        Err(e) => Err(AttachmentError::InvalidAttachment(format!(
            "MP4 rejected (metadata budget: 16 MiB; export a smaller metadata/sample table if exceeded): {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid MP4 boxes, byte-for-byte from mp4san-test's
    // example_ftyp/example_mdat/example_moov fixtures.

    fn ftyp() -> Vec<u8> {
        [
            &[0, 0, 0, 20][..], // box size
            b"ftyp",            // box type
            b"isom",            // major_brand
            &[0, 0, 0, 0],      // minor_version
            b"isom",            // compatible_brands
        ]
        .concat()
    }

    /// ftyp with a non-isom (QuickTime) brand — outside mp4san's scope.
    fn ftyp_qt() -> Vec<u8> {
        [&[0, 0, 0, 20][..], b"ftyp", b"qt  ", &[0, 0, 0, 0], b"qt  "].concat()
    }

    fn mdat() -> Vec<u8> {
        [&[0, 0, 0, 8][..], b"mdat"].concat()
    }

    fn moov() -> Vec<u8> {
        [
            &[0, 0, 0, 56][..], // moov box size
            b"moov",
            &[0, 0, 0, 48], // trak box size
            b"trak",
            &[0, 0, 0, 40], // mdia box size
            b"mdia",
            &[0, 0, 0, 32], // minf box size
            b"minf",
            &[0, 0, 0, 24], // stbl box size
            b"stbl",
            &[0, 0, 0, 16], // stco box size
            b"stco",
            &[0, 0, 0, 0], // version & flags
            &[0, 0, 0, 0], // entry count
        ]
        .concat()
    }

    /// Minimal valid lossless WebP (from webpsan's doc example).
    const VALID_WEBP: &[u8] = b"RIFF\x14\0\0\0WEBPVP8L\x08\0\0\0\x2f\0\0\0\0\x88\x88\x08";

    #[test]
    fn oversized_declared_moov_rejected_before_payload_read() {
        // Only 28 bytes on disk; accepting this declaration would allocate
        // hundreds of MiB before discovering EOF with the old defaults.
        for size in [MAX_MP4_METADATA_SIZE + 9, 512 * 1024 * 1024] {
            let input = [
                ftyp(),
                (size as u32).to_be_bytes().to_vec(),
                b"moov".to_vec(),
            ]
            .concat();
            let dependency_error =
                mp4san::sanitize_with_config(std::io::Cursor::new(&input), mp4_config())
                    .unwrap_err();
            match dependency_error {
                mp4san::Error::Parse(report) => assert!(matches!(
                    report.get_ref(),
                    mp4san::parse::ParseError::InvalidInput
                )),
                other => panic!("expected preallocation size rejection, got {other:?}"),
            }
            let mut file = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut file, &input).unwrap();
            for result in [
                sanitize_media_file(file.path()).map(|_| ()),
                sanitize_media(&input).map(|_| ()),
            ] {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("16 MiB"), "{error}");
                // mp4san reports its size check, rather than an EOF read error.
                assert!(error.contains("metadata"), "{error}");
            }
            assert_eq!(std::fs::read(file.path()).unwrap(), input);
        }
        assert!(check_mp4_metadata_size(MAX_MP4_METADATA_SIZE).is_ok());
        assert!(check_mp4_metadata_size(MAX_MP4_METADATA_SIZE + 1).is_err());
    }

    #[test]
    fn privacy_pass_independently_checks_metadata_budget() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let size = MAX_MP4_METADATA_SIZE + 1;
        file.write_all(&(size as u32).to_be_bytes()).unwrap();
        file.write_all(b"moov").unwrap();
        file.as_file_mut().set_len(size).unwrap();
        let error = strip_mp4_file_privacy_boxes(file.as_file_mut())
            .unwrap_err()
            .to_string();
        assert!(error.contains("16 MiB"), "{error}");
    }

    #[test]
    fn malformed_extended_sizes_are_rejected_without_overflow() {
        let mut movie = moov();
        movie.extend_from_slice(&1u32.to_be_bytes());
        movie.extend_from_slice(b"udta");
        movie.extend_from_slice(&u64::MAX.to_be_bytes());
        let size = movie.len() as u32;
        movie[..4].copy_from_slice(&size.to_be_bytes());
        assert!(strip_mp4_privacy_boxes(&movie).is_err());
        let input = [
            ftyp(),
            1u32.to_be_bytes().to_vec(),
            b"moov".to_vec(),
            u64::MAX.to_be_bytes().to_vec(),
        ]
        .concat();
        assert!(preflight_mp4(&mut std::io::Cursor::new(input)).is_err());
    }

    #[test]
    fn relocation_padding_budget_is_checked_before_dependency_allocation() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&ftyp()).unwrap();
        // Sparse 512 MiB leading free box followed by mdat and trailing moov.
        let padding = 512 * 1024 * 1024u32;
        file.write_all(&padding.to_be_bytes()).unwrap();
        file.write_all(b"free").unwrap();
        file.seek(SeekFrom::Start(ftyp().len() as u64 + padding as u64))
            .unwrap();
        file.write_all(&mdat()).unwrap();
        file.write_all(&moov()).unwrap();
        let error = sanitize_media_file(file.path()).unwrap_err().to_string();
        assert!(error.contains("relocation padding"), "{error}");
        assert!(error.contains("32 MiB"), "{error}");
        // Budget applies only when relocation can build padding: an already
        // streamable file with the same huge free extent stays streamable.
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&ftyp()).unwrap();
        file.write_all(&moov()).unwrap();
        let free_size = padding - moov().len() as u32;
        file.write_all(&free_size.to_be_bytes()).unwrap();
        file.write_all(b"free").unwrap();
        file.as_file_mut()
            .set_len(ftyp().len() as u64 + padding as u64 + 8)
            .unwrap();
        assert!(sanitize_media_file(file.path()).unwrap().is_none());
    }

    #[test]
    fn many_top_level_boxes_stream_without_retaining_a_box_table() {
        use std::io::Write;
        let mut movie = moov();
        movie.extend_from_slice(&12u32.to_be_bytes());
        movie.extend_from_slice(b"udtaGPS!");
        let movie_len = movie.len() as u32;
        movie[..4].copy_from_slice(&movie_len.to_be_bytes());
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&ftyp()).unwrap();
        file.write_all(&movie).unwrap();
        // A million boxes used to require a growing ~24 MiB tuple table.
        let free = [8u32.to_be_bytes().as_slice(), b"free"].concat();
        for _ in 0..1_000_000 {
            file.write_all(&free).unwrap();
        }
        file.write_all(&mdat()).unwrap();
        let original_len = file.as_file().metadata().unwrap().len();
        let output = sanitize_media_file(file.path()).unwrap().unwrap();
        assert_eq!(output.as_file().metadata().unwrap().len(), original_len);
        assert!(sanitize_media_file(output.path()).unwrap().is_none());
        let mut prefix = vec![0; ftyp().len() + movie.len()];
        std::fs::File::open(output.path())
            .unwrap()
            .read_exact(&mut prefix)
            .unwrap();
        assert!(!prefix.windows(4).any(|w| w == b"GPS!" || w == b"udta"));
        assert_eq!(file.as_file().metadata().unwrap().len(), original_len);
    }

    #[test]
    fn file_gps_fixture_matches_bytes_and_revalidates() {
        let input = include_bytes!("../../../tests/fixtures/gps/location.mp4");
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, input).unwrap();
        let output = sanitize_media_file(file.path()).unwrap().unwrap();
        let bytes = std::fs::read(output.path()).unwrap();
        assert_eq!(
            sanitize_media(input).unwrap(),
            SanitizeOutcome::Rewritten(bytes.clone())
        );
        assert!(validate_media(&bytes).is_ok());
        assert_eq!(std::fs::read(file.path()).unwrap(), input);
    }

    #[test]
    fn mp4_with_trailing_moov_is_rewritten_to_sanitized_layout() {
        // moov after mdat: the sanitizer must relocate it before the media data.
        let input = [ftyp(), mdat(), moov()].concat();

        let outcome = sanitize_media(&input).expect("valid mp4 must sanitize");

        let expected = [ftyp(), moov(), mdat()].concat();
        assert_eq!(outcome, SanitizeOutcome::Rewritten(expected.clone()));

        // Round-trip: our own output is already in sanitized layout.
        let outcome2 = sanitize_media(&expected).expect("sanitized output must re-parse");
        assert_eq!(outcome2, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn clean_mp4_passes_unchanged() {
        let input = [ftyp(), moov(), mdat()].concat();
        let outcome = sanitize_media(&input).expect("clean mp4 must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn privacy_erasure_preserves_absolute_media_chunk_offsets() {
        fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            [
                ((8 + payload.len()) as u32).to_be_bytes().as_slice(),
                kind,
                payload,
            ]
            .concat()
        }
        let sample = b"synthetic media sample";
        for wide in [false, true] {
            for trailing_moov in [false, true] {
                // Both privacy box header forms must retain their full extent.
                let private = [
                    atom(b"udta", b"private-location"),
                    [
                        1u32.to_be_bytes().as_slice(),
                        b"meta",
                        &32u64.to_be_bytes(),
                        b"private-metadata",
                    ]
                    .concat(),
                ]
                .concat();
                let make_moov = |offset: u64| {
                    let entry = if wide {
                        offset.to_be_bytes().to_vec()
                    } else {
                        (offset as u32).to_be_bytes().to_vec()
                    };
                    let table = atom(
                        if wide { b"co64" } else { b"stco" },
                        &[vec![0; 4], 1u32.to_be_bytes().to_vec(), entry].concat(),
                    );
                    let track = atom(
                        b"trak",
                        &atom(b"mdia", &atom(b"minf", &atom(b"stbl", &table))),
                    );
                    atom(b"moov", &[track, private.clone()].concat())
                };
                let head = ftyp();
                let sample_offset =
                    head.len() + 8 + if trailing_moov { 0 } else { make_moov(0).len() };
                let movie = make_moov(sample_offset as u64);
                let media = atom(b"mdat", sample);
                let input = if trailing_moov {
                    [head, media, movie].concat()
                } else {
                    [head, movie, media].concat()
                };
                let memory = strip_mp4_privacy_boxes(&input).unwrap().unwrap();
                let mut file = tempfile::NamedTempFile::new().unwrap();
                std::io::Write::write_all(&mut file, &input).unwrap();
                let stripped_file = strip_mp4_file_privacy_boxes(file.as_file_mut())
                    .unwrap()
                    .unwrap();
                let streamed = std::fs::read(stripped_file.path()).unwrap();
                let relocated = sanitize_media_file(file.path()).unwrap().unwrap();
                let relocated_bytes = std::fs::read(relocated.path()).unwrap();
                let expected = sanitize_media(&input).unwrap();
                assert_eq!(
                    expected,
                    SanitizeOutcome::Rewritten(relocated_bytes.clone())
                );
                assert!(validate_media(&relocated_bytes).is_ok());
                let table_kind = if wide { b"co64" } else { b"stco" };
                let table = relocated_bytes
                    .windows(4)
                    .position(|w| w == table_kind)
                    .unwrap();
                let entry = table + 12;
                let relocated_offset = if wide {
                    u64::from_be_bytes(relocated_bytes[entry..entry + 8].try_into().unwrap())
                        as usize
                } else {
                    u32::from_be_bytes(relocated_bytes[entry..entry + 4].try_into().unwrap())
                        as usize
                };
                assert_eq!(
                    &relocated_bytes[relocated_offset..relocated_offset + sample.len()],
                    sample
                );
                for output in [&memory, &streamed] {
                    // Follow the actual chunk table, not a rewritten-length expectation.
                    let kind = if wide { b"co64" } else { b"stco" };
                    let table = output.windows(4).position(|w| w == kind).unwrap();
                    let entry = table + 12;
                    let offset = if wide {
                        u64::from_be_bytes(output[entry..entry + 8].try_into().unwrap()) as usize
                    } else {
                        u32::from_be_bytes(output[entry..entry + 4].try_into().unwrap()) as usize
                    };
                    assert_eq!(offset, sample_offset);
                    assert_eq!(&output[offset..offset + sample.len()], sample);
                    assert_eq!(output.len(), input.len());
                    assert!(!output.windows(8).any(|w| w == b"private-"));
                    assert!(!output.windows(4).any(|w| w == b"udta" || w == b"meta"));
                }
            }
        }
    }

    #[test]
    fn malformed_mp4_is_rejected() {
        // Valid ftyp followed by a moov whose declared size exceeds the input
        // (truncated box).
        let mut input = ftyp();
        input.extend_from_slice(&[0, 0, 1, 0]); // size 256, but no payload
        input.extend_from_slice(b"moov");

        let err = sanitize_media(&input).expect_err("truncated mp4 must be rejected");
        assert!(
            matches!(err, AttachmentError::InvalidAttachment(_)),
            "got: {err:?}"
        );

        // Incoming path applies the same policy.
        assert!(validate_media(&input).is_err());
    }

    #[test]
    fn non_isom_ftyp_passes_through_unchanged() {
        // QuickTime-branded file: sniffs as ftyp but is outside mp4san's scope.
        let input = [ftyp_qt(), mdat(), moov()].concat();
        let outcome = sanitize_media(&input).expect("non-isom ftyp must pass through");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn valid_webp_passes_validation_unchanged() {
        let outcome = sanitize_media(VALID_WEBP).expect("valid webp must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
        assert!(validate_media(VALID_WEBP).is_ok());
    }

    #[test]
    fn malformed_webp_is_rejected() {
        // Correct RIFF/WEBP magic, garbage chunk payload.
        let mut input = b"RIFF\x14\0\0\0WEBP".to_vec();
        input.extend_from_slice(b"VP8L\xff\xff\xff\xffgarbage");

        let err = sanitize_media(&input).expect_err("malformed webp must be rejected");
        assert!(
            matches!(err, AttachmentError::InvalidAttachment(_)),
            "got: {err:?}"
        );
        assert!(validate_media(&input).is_err());
    }

    #[test]
    fn file_variant_rewrites_trailing_moov_and_passes_clean() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Trailing moov → rewritten temp file with sanitized layout.
        let dirty = dir.path().join("dirty.mp4");
        std::fs::write(&dirty, [ftyp(), mdat(), moov()].concat()).unwrap();
        let temp = sanitize_media_file(&dirty)
            .expect("valid mp4 must sanitize")
            .expect("trailing moov must be rewritten");
        assert_eq!(
            std::fs::read(temp.path()).unwrap(),
            [ftyp(), moov(), mdat()].concat()
        );

        // Clean layout → None (upload the original).
        let clean = dir.path().join("clean.mp4");
        std::fs::write(&clean, [ftyp(), moov(), mdat()].concat()).unwrap();
        assert!(
            sanitize_media_file(&clean)
                .expect("clean mp4 must pass")
                .is_none()
        );

        // Malformed → rejected, same policy as the bytes path.
        let bad = dir.path().join("bad.mp4");
        let mut bytes = ftyp();
        bytes.extend_from_slice(&[0, 0, 1, 0]);
        bytes.extend_from_slice(b"moov");
        std::fs::write(&bad, bytes).unwrap();
        assert!(matches!(
            sanitize_media_file(&bad),
            Err(AttachmentError::InvalidAttachment(_))
        ));

        // Non-covered type → None, file untouched.
        let png = dir.path().join("x.png");
        std::fs::write(&png, [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        assert!(
            sanitize_media_file(&png)
                .expect("non-covered must pass")
                .is_none()
        );
    }

    /// Minimal JPEG: SOI, APP0(JFIF), APP1(Exif with a GPS IFD), COM, SOS +
    /// fake entropy data, EOI. Not decodable — structural, for the segment
    /// walker. GPS coords appear as the ASCII rational markers below.
    fn jpeg_with_gps() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        // APP0 JFIF
        let app0_payload = b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0";
        v.extend_from_slice(&[0xFF, 0xE0]);
        v.extend_from_slice(&((app0_payload.len() as u16 + 2).to_be_bytes()));
        v.extend_from_slice(app0_payload);
        // APP1 Exif containing a fake GPS IFD blob (marker string stands in
        // for the rational values; the walker drops the whole segment).
        let mut exif = b"Exif\0\0MM\0*\0\0\0\x08".to_vec();
        exif.extend_from_slice(b"GPSLATITUDE:48.8583N-GPSLONGITUDE:2.2944E");
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&((exif.len() as u16 + 2).to_be_bytes()));
        v.extend_from_slice(&exif);
        // COM segment with a "secret"
        let com = b"shot on my phone at home";
        v.extend_from_slice(&[0xFF, 0xFE]);
        v.extend_from_slice(&((com.len() as u16 + 2).to_be_bytes()));
        v.extend_from_slice(com);
        // SOS + entropy + EOI (walker copies verbatim from SOS on)
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x04, 0x01, 0x00]);
        v.extend_from_slice(&[0xAA; 16]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// PNG: signature, IHDR, eXIf chunk carrying GPS, tEXt secret, IDAT, IEND.
    fn png_with_gps() -> Vec<u8> {
        fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut c = (data.len() as u32).to_be_bytes().to_vec();
            c.extend_from_slice(ty);
            c.extend_from_slice(data);
            let mut crc = crc32(&[&ty[..], data].concat());
            c.extend_from_slice(&crc.to_be_bytes());
            crc = 0;
            let _ = crc; // silence
            c
        }
        // minimal crc32 (IEEE) — tables not worth it for a test
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]));
        v.extend(chunk(b"eXIf", b"MM\0*GPSLATITUDE:48.8583N"));
        v.extend(chunk(b"tEXt", b"Comment\0taken at my house"));
        v.extend(chunk(
            b"IDAT",
            &[0x78, 0x9C, 0x62, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01],
        ));
        v.extend(chunk(b"IEND", b""));
        v
    }

    #[test]
    fn gps_exif_is_stripped_from_outgoing_jpeg_and_png() {
        // JPEG: APP1(Exif/GPS) + COM must be gone; SOI/APP0/SOS/EOI survive.
        let input = jpeg_with_gps();
        let out = match sanitize_media(&input).expect("jpeg must sanitize") {
            SanitizeOutcome::Rewritten(b) => b,
            SanitizeOutcome::Unchanged => input.clone(),
        };
        let hay = |needle: &[u8], hay: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(
            !hay(b"GPSLATITUDE", &out),
            "JPEG GPS EXIF leaked to outgoing bytes"
        );
        assert!(
            !hay(b"shot on my phone", &out),
            "JPEG COM leaked to outgoing bytes"
        );
        assert_eq!(&out[..2], &[0xFF, 0xD8], "SOI must survive");
        assert!(hay(b"JFIF", &out), "APP0/JFIF must survive (not metadata)");
        assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9], "EOI must survive");

        // PNG: eXIf + tEXt must be gone; IHDR/IDAT/IEND survive.
        let input = png_with_gps();
        let out = match sanitize_media(&input).expect("png must sanitize") {
            SanitizeOutcome::Rewritten(b) => b,
            SanitizeOutcome::Unchanged => input.clone(),
        };
        assert!(
            !hay(b"GPSLATITUDE", &out),
            "PNG eXIf GPS leaked to outgoing bytes"
        );
        assert!(
            !hay(b"taken at my house", &out),
            "PNG tEXt leaked to outgoing bytes"
        );
        assert!(hay(b"IHDR", &out) && hay(b"IDAT", &out) && hay(b"IEND", &out));

        // Incoming validate must NOT reject clean-but-unstripped media (it is
        // the sender's job to strip; receiving GPS'd media stays displayable).
        assert!(validate_media(&jpeg_with_gps()).is_ok());
    }

    /// Real, decodable GPS-tagged fixtures, generated from a black 16x16 frame
    /// with FFmpeg; JPEG/PNG GPS EXIF was added with ExifTool. No external
    /// services, tools or GPS_SAMPLES_DIR are needed to run this regression.
    #[test]
    fn real_gps_samples_are_stripped_and_still_decode() {
        let samples: [(&str, &[u8]); 3] = [
            (
                "jpg",
                include_bytes!("../../../tests/fixtures/gps/location.jpg"),
            ),
            (
                "png",
                include_bytes!("../../../tests/fixtures/gps/location.png"),
            ),
            (
                "mp4",
                include_bytes!("../../../tests/fixtures/gps/location.mp4"),
            ),
        ];
        let hay = |needle: &[u8], bytes: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        for (ext, input) in samples {
            let out = match sanitize_media(input).expect("GPS fixture must sanitize") {
                SanitizeOutcome::Rewritten(bytes) => bytes,
                SanitizeOutcome::Unchanged => panic!("{ext}: GPS fixture was not stripped"),
            };
            if ext != "mp4" {
                assert!(out.len() < input.len(), "{ext}: metadata must be removed");
            }
            assert!(
                validate_media(&out).is_ok(),
                "{ext}: sanitized media must remain valid"
            );
            if ext == "mp4" {
                assert!(
                    hay(b"udta", input),
                    "fixture must exercise moov.udta stripping"
                );
                assert!(!hay(b"udta", &out), "MP4 private metadata survived");
                assert!(!hay(b"+48.8583", &out), "MP4 GPS location survived");
            } else {
                // JPEG EXIF APP1 and PNG eXIf both carry binary GPS rationals;
                // testing just an ASCII coordinate would miss those encodings.
                if ext == "jpg" {
                    assert!(hay(b"Exif", input));
                    assert!(!hay(b"Exif", &out));
                } else {
                    assert!(hay(b"eXIf", input));
                    assert!(!hay(b"eXIf", &out));
                }
                image::load_from_memory(&out).expect("stripped image must still decode");
            }
        }
    }

    #[test]
    fn non_covered_type_passes_through_byte_identical() {
        // PDF magic + arbitrary bytes: not MP4/WebP/JPEG/PNG — never touched.
        // (PNG previously played this role; it is now a covered format.)
        let input: Vec<u8> = [&b"%PDF-1.7\n"[..], b"arbitrary-pdf-ish-payload"].concat();
        let before = input.clone();

        let outcome = sanitize_media(&input).expect("non-covered type must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
        assert_eq!(input, before, "pass-through must not mutate input");
        assert!(validate_media(&input).is_ok());
    }
}

#[cfg(test)]
#[path = "sanitize_stream_tests.rs"]
mod streaming_tests;
