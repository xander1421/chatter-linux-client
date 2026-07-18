//! Attachment media sanitization via Signal's `signal-media` (mp4san + webpsan).
//!
//! Two hooks, mirroring how Signal clients use these sanitizers:
//!
//! - **Outgoing** ([`sanitize_media`]): before upload/encryption. MP4 presentation
//!   metadata is normalized (`moov` relocated before `mdat`, non-presentation
//!   metadata boxes stripped) and WebP structure is validated. Malformed files
//!   are rejected — we never emit bytes we could not parse.
//! - **Incoming** ([`validate_media`]): after decryption, before the plaintext is
//!   handed to renderers (image crate, FFmpeg). Malformed MP4/WebP is rejected so
//!   crafted files never reach the platform media parsers.
//!
//! Detection is content-based (magic bytes), not MIME-based, because our
//! renderers also sniff content — a lying MIME type must not bypass validation.
//!
//! ponytail: coverage ceiling — signal-media covers exactly ISO-BMFF MP4
//! (`isom`-family brands) and WebP. Everything else (JPEG/PNG/GIF EXIF, MOV,
//! HEIC, MKV, PDF, …) passes through byte-identical; we deliberately do NOT
//! write custom sanitizers for other formats. Non-`isom` ftyp files (HEIC
//! photos, QuickTime .mov) are outside mp4san's supported scope and also pass
//! through unchanged.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use signal_media::sanitize::{mp4, webp};

use super::AttachmentError;

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
/// the rewritten MP4 when the metadata had to be relocated/stripped (caller
/// encrypts the temp file instead of the original); `None` when the original
/// can be uploaded as-is.
///
/// Blocking file IO — call from `spawn_blocking`.
pub fn sanitize_media_file(
    path: &Path,
) -> Result<Option<tempfile::NamedTempFile>, AttachmentError> {
    let open_err = |e| AttachmentError::FileError(format!("Failed to open file for sanitizing: {e}"));
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

    if !looks_like_mp4(&magic[..n]) {
        return Ok(None);
    }

    // mp4san's sync API over the file directly (mediasan impls Skip for File).
    let sanitized = match mp4san::sanitize(BufReader::new(&mut file)) {
        Ok(s) => s,
        // Same carve-out as the bytes path: non-isom ftyp is outside scope.
        Err(mp4san::Error::Parse(report))
            if matches!(report.get_ref(), mp4san::parse::ParseError::UnsupportedFormat(_)) =>
        {
            tracing::debug!("ftyp file outside mp4san scope, passing through");
            return Ok(None);
        }
        Err(e) => {
            return Err(AttachmentError::InvalidAttachment(format!(
                "malformed MP4 rejected: {e}"
            )))
        }
    };

    let Some(metadata) = sanitized.metadata else {
        // Metadata already clean and ahead of the media data.
        return Ok(None);
    };

    super::ensure_attachments_dir()
        .map_err(|e| AttachmentError::FileError(format!("Failed to create attachments dir: {e}")))?;
    let mut out = tempfile::NamedTempFile::new_in(super::attachments_dir())
        .map_err(|e| AttachmentError::FileError(format!("Failed to create sanitize temp: {e}")))?;

    use std::io::Write;
    out.write_all(&metadata)
        .map_err(|e| AttachmentError::FileError(format!("Failed to write sanitized metadata: {e}")))?;
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

async fn sanitize_mp4(content: &[u8]) -> Result<SanitizeOutcome, AttachmentError> {
    match mp4::sanitize(futures::io::Cursor::new(content), None).await {
        Ok(sanitized) => match sanitized.metadata {
            Some(metadata) => {
                let start = usize::try_from(sanitized.data.offset).map_err(|_| {
                    AttachmentError::InvalidAttachment("MP4 data offset overflow".into())
                })?;
                let len = usize::try_from(sanitized.data.len).map_err(|_| {
                    AttachmentError::InvalidAttachment("MP4 data length overflow".into())
                })?;
                let end = start.checked_add(len).filter(|&e| e <= content.len()).ok_or_else(
                    || AttachmentError::InvalidAttachment("MP4 data span out of bounds".into()),
                )?;

                let mut output = Vec::with_capacity(metadata.len() + len);
                output.extend_from_slice(&metadata);
                output.extend_from_slice(&content[start..end]);
                Ok(SanitizeOutcome::Rewritten(output))
            }
            // Metadata already clean and ahead of the media data.
            None => Ok(SanitizeOutcome::Unchanged),
        },
        // Not an isom-brand MP4 (HEIC photo, QuickTime .mov, …): outside
        // mp4san's supported scope — treated as a non-covered format.
        Err(mp4::Error::Parse(report))
            if matches!(report.kind, mp4::ParseError::UnsupportedFormat(_)) =>
        {
            tracing::debug!(kind = %report.kind, "ftyp file outside mp4san scope, passing through");
            Ok(SanitizeOutcome::Unchanged)
        }
        Err(e) => Err(AttachmentError::InvalidAttachment(format!(
            "malformed MP4 rejected: {e}"
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
        [
            &[0, 0, 0, 20][..],
            b"ftyp",
            b"qt  ",
            &[0, 0, 0, 0],
            b"qt  ",
        ]
        .concat()
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
    fn malformed_mp4_is_rejected() {
        // Valid ftyp followed by a moov whose declared size exceeds the input
        // (truncated box).
        let mut input = ftyp();
        input.extend_from_slice(&[0, 0, 1, 0]); // size 256, but no payload
        input.extend_from_slice(b"moov");

        let err = sanitize_media(&input).expect_err("truncated mp4 must be rejected");
        assert!(matches!(err, AttachmentError::InvalidAttachment(_)), "got: {err:?}");

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
        assert!(matches!(err, AttachmentError::InvalidAttachment(_)), "got: {err:?}");
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
        assert_eq!(std::fs::read(temp.path()).unwrap(), [ftyp(), moov(), mdat()].concat());

        // Clean layout → None (upload the original).
        let clean = dir.path().join("clean.mp4");
        std::fs::write(&clean, [ftyp(), moov(), mdat()].concat()).unwrap();
        assert!(sanitize_media_file(&clean).expect("clean mp4 must pass").is_none());

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
        assert!(sanitize_media_file(&png).expect("non-covered must pass").is_none());
    }

    #[test]
    fn non_covered_type_passes_through_byte_identical() {
        // PNG magic + arbitrary bytes: not MP4, not WebP — never touched.
        let input: Vec<u8> = [
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A][..],
            b"arbitrary-not-actually-a-png-payload",
        ]
        .concat();
        let before = input.clone();

        let outcome = sanitize_media(&input).expect("non-covered type must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
        assert_eq!(input, before, "pass-through must not mutate input");
        assert!(validate_media(&input).is_ok());
    }
}
