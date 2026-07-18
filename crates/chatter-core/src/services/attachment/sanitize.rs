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
pub async fn sanitize_media(content: &[u8]) -> Result<SanitizeOutcome, AttachmentError> {
    if looks_like_mp4(content) {
        return sanitize_mp4(content).await;
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
/// Sync on purpose — the decrypt paths are sync. The mp4 sanitizer is async but
/// performs no real I/O over an in-memory cursor, so `block_on` never parks.
pub fn validate_media(content: &[u8]) -> Result<(), AttachmentError> {
    futures::executor::block_on(sanitize_media(content)).map(|_| ())
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

    use futures::executor::block_on;

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

        let outcome = block_on(sanitize_media(&input)).expect("valid mp4 must sanitize");

        let expected = [ftyp(), moov(), mdat()].concat();
        assert_eq!(outcome, SanitizeOutcome::Rewritten(expected.clone()));

        // Round-trip: our own output is already in sanitized layout.
        let outcome2 = block_on(sanitize_media(&expected)).expect("sanitized output must re-parse");
        assert_eq!(outcome2, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn clean_mp4_passes_unchanged() {
        let input = [ftyp(), moov(), mdat()].concat();
        let outcome = block_on(sanitize_media(&input)).expect("clean mp4 must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn malformed_mp4_is_rejected() {
        // Valid ftyp followed by a moov whose declared size exceeds the input
        // (truncated box).
        let mut input = ftyp();
        input.extend_from_slice(&[0, 0, 1, 0]); // size 256, but no payload
        input.extend_from_slice(b"moov");

        let err = block_on(sanitize_media(&input)).expect_err("truncated mp4 must be rejected");
        assert!(matches!(err, AttachmentError::InvalidAttachment(_)), "got: {err:?}");

        // Incoming path applies the same policy.
        assert!(validate_media(&input).is_err());
    }

    #[test]
    fn non_isom_ftyp_passes_through_unchanged() {
        // QuickTime-branded file: sniffs as ftyp but is outside mp4san's scope.
        let input = [ftyp_qt(), mdat(), moov()].concat();
        let outcome = block_on(sanitize_media(&input)).expect("non-isom ftyp must pass through");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
    }

    #[test]
    fn valid_webp_passes_validation_unchanged() {
        let outcome = block_on(sanitize_media(VALID_WEBP)).expect("valid webp must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
        assert!(validate_media(VALID_WEBP).is_ok());
    }

    #[test]
    fn malformed_webp_is_rejected() {
        // Correct RIFF/WEBP magic, garbage chunk payload.
        let mut input = b"RIFF\x14\0\0\0WEBP".to_vec();
        input.extend_from_slice(b"VP8L\xff\xff\xff\xffgarbage");

        let err = block_on(sanitize_media(&input)).expect_err("malformed webp must be rejected");
        assert!(matches!(err, AttachmentError::InvalidAttachment(_)), "got: {err:?}");
        assert!(validate_media(&input).is_err());
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

        let outcome = block_on(sanitize_media(&input)).expect("non-covered type must pass");
        assert_eq!(outcome, SanitizeOutcome::Unchanged);
        assert_eq!(input, before, "pass-through must not mutate input");
        assert!(validate_media(&input).is_ok());
    }
}
