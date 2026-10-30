//! File-streaming regressions. The existing bytes parsers are an independent
//! compatibility oracle; tests also decode the real stripped fixture pixels.
use super::*;
use std::io::{self, Cursor, Write};

const JPEG: &[u8] = include_bytes!("../../../tests/fixtures/gps/location.jpg");
const PNG: &[u8] = include_bytes!("../../../tests/fixtures/gps/location.png");

fn stream(input: &[u8], jpeg: bool) -> Result<Option<Vec<u8>>, AttachmentError> {
    let mut output = Vec::new();
    // Exercise partial reads as well as the usual file reader.
    let mut reader = GuardedReader::new(Cursor::new(input), 7);
    let dropped = if jpeg {
        stream_jpeg_metadata(&mut reader, &mut output, input.len() as u64)?
    } else {
        stream_png_metadata(&mut reader, &mut output, input.len() as u64)?
    };
    Ok(dropped.then_some(output))
}

fn assert_parity(input: &[u8], jpeg: bool) {
    let bytes = if jpeg {
        strip_jpeg_metadata(input)
    } else {
        strip_png_metadata(input)
    };
    match (bytes, stream(input, jpeg)) {
        (Ok(expected), Ok(actual)) => assert_eq!(actual, expected),
        (
            Err(AttachmentError::InvalidAttachment(_)),
            Err(AttachmentError::InvalidAttachment(_)),
        ) => {}
        (expected, actual) => {
            panic!("parser disagreement: bytes={expected:?}, streamed={actual:?}")
        }
    }
}

fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    [
        &[0xFF, marker][..],
        &(u16::try_from(payload.len()).unwrap() + 2).to_be_bytes(),
        payload,
    ]
    .concat()
}

// Deliberately opaque CRCs: baseline preserves retained CRC bytes without
// checking them. Fixture decode and byte equality also cover real valid CRCs.
fn png_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [
        &(payload.len() as u32).to_be_bytes()[..],
        kind,
        payload,
        &[0x12, 0x34, 0x56, 0x78],
    ]
    .concat()
}

#[test]
fn file_images_match_bytes_remove_metadata_and_preserve_pixels() {
    let dir = tempfile::tempdir().unwrap();
    for (name, input, jpeg) in [("photo.jpg", JPEG, true), ("photo.png", PNG, false)] {
        let source = dir.path().join(name);
        std::fs::write(&source, input).unwrap();
        let expected = if jpeg {
            strip_jpeg_metadata(input)
        } else {
            strip_png_metadata(input)
        }
        .unwrap()
        .expect("GPS fixture must be rewritten");
        let temp = sanitize_media_file(&source).unwrap().unwrap();
        let actual = std::fs::read(temp.path()).unwrap();
        assert_eq!(actual, expected);
        assert!(actual.len() < input.len());
        assert!(
            !actual
                .windows(4)
                .any(|w| w == if jpeg { b"Exif" } else { b"eXIf" })
        );
        let before = image::load_from_memory(input).unwrap().to_rgba8();
        let after = image::load_from_memory(&actual).unwrap().to_rgba8();
        assert_eq!(after, before, "pixel data must survive lossless surgery");
        assert_eq!(std::fs::read(&source).unwrap(), input);
        let temp_path = temp.path().to_path_buf();
        drop(temp);
        assert!(!temp_path.exists());
        std::fs::write(&source, &actual).unwrap();
        assert!(sanitize_media_file(&source).unwrap().is_none());
        assert_eq!(std::fs::read(&source).unwrap(), actual);
    }
}

#[test]
fn jpeg_marker_policy_and_every_truncated_prefix_match_bytes() {
    let mut input = vec![0xFF, 0xD8];
    for marker in [0xE0, 0xE1, 0xE2, 0xFE] {
        input.extend(jpeg_segment(marker, &[marker; 9]));
    }
    input.extend([0xFF, 0x01, 0xFF, 0xD0]); // TEM and RST0
    input.extend([0xFF, 0xDA, 0, 4, 1, 0, 0xFF, 0, 0xFF, 0xD9]);
    // APP1-looking bytes after SOS belong to the opaque tail, not our parser.
    input.extend(jpeg_segment(0xE1, b"opaque scan tail"));
    for end in 2..=input.len() {
        assert_parity(&input[..end], true);
    }
    let output = stream(&input, true).unwrap().unwrap();
    // Check exact baseline result, including APP2/ICC and the opaque tail.
    assert_eq!(output, strip_jpeg_metadata(&input).unwrap().unwrap());
    assert!(output.windows(16).any(|w| w == b"opaque scan tail"));
    for marker in [0xDA, 0xD9] {
        assert_parity(&[0xFF, 0xD8, 0xFF, marker, 0, 0], true);
    }
    for bad_len in [0u16, 1, 0xFFFF] {
        let input = [&[0xFF, 0xD8, 0xFF, 0xE1][..], &bad_len.to_be_bytes()].concat();
        assert_parity(&input, true);
    }
    assert_parity(&[0xFF, 0xD8, 0, 0, 0, 0], true);
}

#[test]
fn png_chunk_policy_crcs_iend_and_truncation_match_bytes() {
    let mut input = PNG_SIG.to_vec();
    for kind in [
        b"IHDR", b"eXIf", b"tEXt", b"zTXt", b"iTXt", b"tIME", b"iCCP", b"IDAT",
    ] {
        input.extend(png_chunk(kind, b"payload"));
    }
    input.extend(png_chunk(b"IEND", b""));
    // A rewrite ends at IEND; without a dropped chunk, upload uses original
    // including trailing bytes. Preserve both branches of this existing rule.
    input.extend(b"unparsed trailer");
    for end in 8..=input.len() {
        assert_parity(&input[..end], false);
    }
    let output = stream(&input, false).unwrap().unwrap();
    let expected = [
        PNG_SIG.to_vec(),
        png_chunk(b"IHDR", b"payload"),
        png_chunk(b"iCCP", b"payload"),
        png_chunk(b"IDAT", b"payload"),
        png_chunk(b"IEND", b""),
    ]
    .concat();
    assert_eq!(
        output, expected,
        "retained chunks/CRCs must be byte-identical"
    );
    let clean = [
        PNG_SIG.to_vec(),
        png_chunk(b"IEND", b""),
        b"trailer".to_vec(),
    ]
    .concat();
    assert_parity(&clean, false);
    assert!(stream(&clean, false).unwrap().is_none());
    // Maximum u32 length must be rejected before any chunk-sized read or
    // usize addition, including on 32-bit Android.
    for kind in [b"IDAT", b"tEXt"] {
        let huge = [PNG_SIG.as_slice(), &u32::MAX.to_be_bytes(), kind].concat();
        assert_parity(&huge, false);
    }
}

#[test]
fn malformed_file_images_are_rejected_without_touching_sources() {
    let dir = tempfile::tempdir().unwrap();
    let jpeg = [
        vec![0xFF, 0xD8],
        jpeg_segment(0xE1, b"private metadata"),
        vec![0xFF, 0xE2, 0xFF, 0xFF], // length extends beyond the file
    ]
    .concat();
    let png = [
        PNG_SIG.to_vec(),
        png_chunk(b"tEXt", b"private metadata"),
        vec![0, 0, 0, 20],
        b"IDAT".to_vec(), // missing payload and CRC
    ]
    .concat();
    for (name, input) in [("bad.jpg", jpeg), ("bad.png", png)] {
        let source = dir.path().join(name);
        std::fs::write(&source, &input).unwrap();
        assert!(matches!(
            sanitize_media_file(&source),
            Err(AttachmentError::InvalidAttachment(_))
        ));
        assert_eq!(std::fs::read(&source).unwrap(), input);
    }
}

struct GuardedReader<R> {
    inner: R,
    max_return: usize,
    max_requested: usize,
    read: u64,
}

impl<R> GuardedReader<R> {
    fn new(inner: R, max_return: usize) -> Self {
        Self {
            inner,
            max_return,
            max_requested: 0,
            read: 0,
        }
    }
}

impl<R: Read> Read for GuardedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        assert!(
            buf.len() <= IMAGE_COPY_BUFFER_SIZE,
            "unbounded read requested"
        );
        self.max_requested = self.max_requested.max(buf.len());
        let count = buf.len().min(self.max_return);
        let got = self.inner.read(&mut buf[..count])?;
        self.read += got as u64;
        Ok(got)
    }
}

#[derive(Default)]
struct CountingWriter(u64);
impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        assert!(
            buf.len() <= IMAGE_COPY_BUFFER_SIZE,
            "unbounded write requested"
        );
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn large_kept_dropped_chunks_and_jpeg_tail_use_bounded_reads() {
    const LARGE: u64 = 32 * 1024 * 1024;
    for kind in [b"IDAT", b"tEXt"] {
        let head = [PNG_SIG.as_slice(), &(LARGE as u32).to_be_bytes(), kind].concat();
        let input = Cursor::new(head).chain(io::repeat(0).take(LARGE + 4));
        let mut input = GuardedReader::new(input, usize::MAX);
        let mut output = CountingWriter::default();
        let dropped = stream_png_metadata(&mut input, &mut output, LARGE + 20).unwrap();
        assert_eq!(dropped, kind == b"tEXt");
        assert_eq!(input.read, LARGE + 20);
        assert_eq!(input.max_requested, IMAGE_COPY_BUFFER_SIZE);
        assert_eq!(output.0, if dropped { 8 } else { LARGE + 20 });
    }
    for marker in [0xE1, 0xE2] {
        let head = [0xFF, 0xD8, 0xFF, marker, 0xFF, 0xFF];
        let input = Cursor::new(head)
            .chain(io::repeat(0).take(u64::from(u16::MAX) - 2))
            .chain(Cursor::new([0xFF, 0xDA, 0, 0]));
        let len = u64::from(u16::MAX) + 8;
        let mut input = GuardedReader::new(input, usize::MAX);
        let mut output = CountingWriter::default();
        let dropped = stream_jpeg_metadata(&mut input, &mut output, len).unwrap();
        assert_eq!(dropped, marker == 0xE1);
        assert_eq!(input.read, len);
        assert_eq!(input.max_requested, IMAGE_COPY_BUFFER_SIZE);
        assert_eq!(output.0, if dropped { 6 } else { len });
    }
    let head = [0xFF, 0xD8, 0xFF, 0xDA];
    let input = Cursor::new(head).chain(io::repeat(0).take(LARGE));
    let mut input = GuardedReader::new(input, usize::MAX);
    let mut output = CountingWriter::default();
    assert!(!stream_jpeg_metadata(&mut input, &mut output, LARGE + 4).unwrap());
    assert_eq!(input.read, LARGE + 4);
    assert_eq!(input.max_requested, IMAGE_COPY_BUFFER_SIZE);
    assert_eq!(output.0, LARGE + 4);
}

struct FailingReader;
impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected read error"))
    }
}
struct FailingWriter;
impl Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected write error"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn partial_temps_are_unlinked_on_errors_and_unchanged_images() {
    let dir = tempfile::tempdir().unwrap();
    let entries = || std::fs::read_dir(dir.path()).unwrap().count();
    for (input, jpeg) in [(JPEG, true), (PNG, false)] {
        let stripped = stream(input, jpeg).unwrap().unwrap();
        assert!(
            sanitize_image_to_temp(
                &mut Cursor::new(&stripped),
                stripped.len() as u64,
                jpeg,
                dir.path()
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(entries(), 0, "unchanged output must be discarded");
        // Claim the full length but stop after metadata parsing began, testing
        // actual EOF detection even if the source shrinks after fstat.
        assert!(matches!(
            sanitize_image_to_temp(
                &mut Cursor::new(&input[..input.len() / 2]),
                input.len() as u64,
                jpeg,
                dir.path()
            ),
            Err(AttachmentError::InvalidAttachment(_))
        ));
        assert_eq!(entries(), 0, "parse failure leaked a partial temp");
        let mut reader = Cursor::new(&input[..input.len() / 2]).chain(FailingReader);
        assert!(matches!(
            sanitize_image_to_temp(&mut reader, input.len() as u64, jpeg, dir.path()),
            Err(AttachmentError::FileError(_))
        ));
        assert_eq!(entries(), 0, "read failure leaked a partial temp");
        let result = if jpeg {
            stream_jpeg_metadata(
                &mut Cursor::new(input),
                &mut FailingWriter,
                input.len() as u64,
            )
        } else {
            stream_png_metadata(
                &mut Cursor::new(input),
                &mut FailingWriter,
                input.len() as u64,
            )
        };
        assert!(matches!(result, Err(AttachmentError::FileError(_))));
    }
}

/// Explicit opt-in: reads a 2 GiB sparse metadata chunk while writing only the
/// signature and IEND. Run under a memory limit to catch accidental buffering.
#[test]
#[ignore = "2 GiB sparse IO stress; run explicitly under a memory limit"]
fn sparse_two_gib_png_metadata_is_streamed_and_source_is_untouched() {
    const LARGE: u64 = 2 * 1024 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("large.png");
    let mut file = std::fs::File::create(&source).unwrap();
    file.write_all(&PNG_SIG).unwrap();
    file.write_all(&(LARGE as u32).to_be_bytes()).unwrap();
    file.write_all(b"tEXt").unwrap();
    file.seek(SeekFrom::Current(LARGE as i64)).unwrap();
    file.write_all(&[0; 4]).unwrap(); // CRC remains opaque, as in the bytes path.
    file.write_all(&png_chunk(b"IEND", b"")).unwrap();
    file.flush().unwrap();
    let source_len = file.metadata().unwrap().len();
    drop(file);
    let temp = sanitize_media_file(&source).unwrap().unwrap();
    assert_eq!(
        std::fs::read(temp.path()).unwrap(),
        [PNG_SIG.to_vec(), png_chunk(b"IEND", b"")].concat()
    );
    assert_eq!(std::fs::metadata(&source).unwrap().len(), source_len);
    let mut original = std::fs::File::open(&source).unwrap();
    let mut head = [0; 16];
    original.read_exact(&mut head).unwrap();
    assert_eq!(&head[..8], &PNG_SIG);
    assert_eq!(&head[12..], b"tEXt");
    original.seek(SeekFrom::Start(source_len - 12)).unwrap();
    let mut tail = [0; 12];
    original.read_exact(&mut tail).unwrap();
    assert_eq!(tail.as_slice(), png_chunk(b"IEND", b""));
}
