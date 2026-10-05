//! Attachment domain entity.
//!
//! Represents file attachments and metadata.

use crate::domain::ids::AttachmentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Attachment status in upload/download lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AttachmentStatus {
    #[default]
    Pending,
    Uploading,
    Downloading,
    Processing,
    Ready,
    Failed,
    Expired,
}

/// Where the encrypted content is stored.
///
/// Tracks whether encrypted attachment data needs to be fetched from CDN
/// or is already stored locally (in database or filesystem).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ContentLocation {
    /// Content needs to be fetched from CDN (not downloaded yet).
    #[default]
    NotDownloaded = 0,
    /// Content is stored encrypted in database BLOB (legacy, for migration compatibility).
    InDatabase = 1,
    /// Content is stored encrypted on filesystem (~/.local/share/chatter/attachments/).
    /// This is the preferred storage for large files (videos, large images).
    OnFilesystem = 2,
}

/// File category for UI display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileCategory {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Code,
    Other,
}

impl FileCategory {
    /// Lowercase name used wherever a category crosses into presentation
    /// (Slint bindings, `ui::file_data::category_icon`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Code => "code",
            Self::Other => "other",
        }
    }

    /// Infer category from MIME type.
    #[must_use]
    pub fn from_mime_type(mime: &str) -> Self {
        if mime.starts_with("image/") {
            Self::Image
        } else if mime.starts_with("video/") {
            Self::Video
        } else if mime.starts_with("audio/") {
            Self::Audio
        } else if mime.starts_with("text/") || mime.contains("javascript") || mime.contains("json")
        {
            Self::Code
        } else if mime == "application/pdf"
            || mime.contains("document")
            || mime.contains("sheet")
            || mime.contains("presentation")
        {
            Self::Document
        } else if mime.contains("zip") || mime.contains("tar") || mime.contains("compressed") {
            Self::Archive
        } else {
            Self::Other
        }
    }

    /// Infer category from filename extension.
    #[must_use]
    pub fn from_filename(filename: &str) -> Self {
        let ext = std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            // Images
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "ico" => Self::Image,
            // Video
            "mp4" | "webm" | "mkv" | "avi" | "mov" | "wmv" | "flv" => Self::Video,
            // Audio
            "mp3" | "wav" | "ogg" | "flac" | "aac" | "m4a" | "wma" => Self::Audio,
            // Code/Text
            "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "go" | "java" | "c" | "cpp" | "h"
            | "hpp" | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "html" | "css" | "scss"
            | "sass" | "less" | "json" | "yaml" | "yml" | "toml" | "xml" | "md" | "markdown"
            | "txt" | "log" | "conf" | "cfg" | "sh" | "bash" | "zsh" | "fish" | "sql"
            | "graphql" | "proto" | "dockerfile" | "makefile" | "cmake" | "gitignore" | "env"
            | "ini" | "vue" | "svelte" => Self::Code,
            // Documents
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" => {
                Self::Document
            }
            // Archives
            "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" => Self::Archive,
            // Unknown
            _ => Self::Other,
        }
    }

    /// Infer category from both MIME type and filename, preferring more specific matches.
    #[must_use]
    pub fn from_mime_and_filename(mime: &str, filename: &str) -> Self {
        let from_mime = Self::from_mime_type(mime);
        let from_ext = Self::from_filename(filename);

        // If MIME type gives us something specific, use it
        if from_mime != Self::Other {
            return from_mime;
        }

        // Otherwise fall back to extension
        from_ext
    }
}

/// Which tab of a conversation's shared-content view an item belongs to.
///
/// Deliberately NOT [`FileCategory`]: that one answers "what kind of file is
/// this" for a single attachment bubble. A shared-content view asks a
/// different question and has two things `FileCategory` cannot express — a
/// link is not an attachment at all, and the Audio bucket has to split into
/// voice notes and music the way every messenger's media drawer does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedCategory {
    /// Photos and videos — the grid.
    Media,
    /// Any other attachment: documents, archives, code.
    File,
    /// A URL found in message text. Has no attachment record behind it.
    Link,
    /// An audio attachment recorded in-app — it carries a waveform.
    Voice,
    /// An audio attachment that is not a voice note.
    Music,
    /// `image/gif`, split out of Media the way every messenger does.
    Gif,
    /// Transient: an attachment whose `attachments` row has not landed yet,
    /// so its MIME type is not known. Never rendered — `AttachmentRepository`
    /// resolves it to a real category when the row arrives. See the
    /// `shared_content` migration for why both write orders happen.
    Unsorted,
}

impl SharedCategory {
    /// Lowercase name — the value stored in `shared_content.category` and the
    /// tab id the UI sends back.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::File => "file",
            Self::Link => "link",
            Self::Voice => "voice",
            Self::Music => "music",
            Self::Gif => "gif",
            Self::Unsorted => "unsorted",
        }
    }

    /// Parse a stored/UI category name. Unknown names are `None` rather than
    /// a silent default — a typo'd tab id should show nothing, not photos.
    #[must_use]
    pub fn from_str_value(s: &str) -> Option<Self> {
        Some(match s {
            "media" => Self::Media,
            "file" => Self::File,
            "link" => Self::Link,
            "voice" => Self::Voice,
            "music" => Self::Music,
            "gif" => Self::Gif,
            "unsorted" => Self::Unsorted,
            _ => return None,
        })
    }

    /// Which tab an attachment belongs in.
    ///
    /// Order matters: GIF is checked before Media (a GIF is an image), and
    /// the waveform check before Music (a voice note is audio).
    #[must_use]
    pub fn of(meta: &FileMetadata) -> Self {
        if meta.is_gif() {
            return Self::Gif;
        }
        if meta.is_audio() {
            // Stored attachment metadata classifies voice by waveform; the
            // recorder supplies it even without a platform waveform decoder.
            return if meta.waveform_samples.is_some() {
                Self::Voice
            } else {
                Self::Music
            };
        }
        match FileCategory::from_mime_and_filename(&meta.mime_type, &meta.name) {
            FileCategory::Image | FileCategory::Video => Self::Media,
            _ => Self::File,
        }
    }
}

/// Pull the URLs out of a message's text.
///
/// Deliberately a scheme-prefix scan over whitespace-separated words, not a
/// regex: the Links tab wants "things the user can tap", and anything that
/// does not start `http://` or `https://` is not that. Trailing sentence
/// punctuation is trimmed so "see https://x.dev." does not store the period.
#[must_use]
pub fn extract_links(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .map(|w| w.trim_end_matches(['.', ',', ')', ']', '>', '!', '?', ';', ':', '"', '\'']))
        .filter(|w| w.len() > "https://".len())
        .map(str::to_string)
        .collect()
}

/// Represents a file attachment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub metadata: FileMetadata,
    pub status: AttachmentStatus,
    /// Where encrypted content is stored (NotDownloaded or InDatabase).
    #[serde(default)]
    pub content_location: ContentLocation,
    /// Local file path (DEPRECATED - kept for migration compatibility).
    /// New attachments should use content_location + encrypted_content in DB.
    #[serde(default)]
    pub local_path: Option<String>,
    /// Remote URL / CDN key for downloading encrypted content.
    pub remote_url: Option<String>,
    /// Encryption key for encrypted attachments (64 bytes: AES key + HMAC key).
    pub encryption_key: Option<Vec<u8>>,
    /// Encryption IV/digest for verification (32 bytes SHA-256).
    pub encryption_iv: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
}

impl Attachment {
    /// Whether this attachment has what it needs to fetch + decrypt its remote
    /// content: a CDN URL and an encryption key. Callers use this to decide
    /// whether to enqueue a background download.
    #[must_use]
    pub fn is_downloadable(&self) -> bool {
        self.remote_url.is_some() && self.encryption_key.is_some()
    }

    /// Create a new pending attachment.
    #[must_use]
    pub fn new(id: AttachmentId, metadata: FileMetadata) -> Self {
        Self {
            id,
            metadata,
            status: AttachmentStatus::Pending,
            content_location: ContentLocation::NotDownloaded,
            local_path: None,
            remote_url: None,
            encryption_key: None,
            encryption_iv: None,
            created_at: Utc::now(),
        }
    }

    /// Check if encrypted content is available locally (on filesystem).
    #[must_use]
    pub fn has_local_content(&self) -> bool {
        self.content_location == ContentLocation::OnFilesystem
    }

    /// Check if content needs to be downloaded from CDN.
    #[must_use]
    pub fn needs_download(&self) -> bool {
        matches!(self.content_location, ContentLocation::NotDownloaded)
            && self.remote_url.is_some()
            && self.encryption_key.is_some()
    }

    /// Check if attachment is ready for use.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.status, AttachmentStatus::Ready)
    }

    /// Check if attachment is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.encryption_key.is_some()
    }

    /// Rebuild the `AttachmentPointer` proto that a sender ships inside the
    /// E2E message payload. Returns `None` when the local record lacks the
    /// CDN key or the per-blob AES key — without both, receivers cannot
    /// download and decrypt the blob.
    #[must_use]
    pub fn to_pointer(&self) -> Option<crate::proto::content_v1::AttachmentPointer> {
        let cdn_key = self.remote_url.clone()?;
        let encryption_key = self.encryption_key.clone()?;
        // Digest (SHA-256 over ciphertext) is REQUIRED for integrity verification
        // on the receiver side — defaulting to empty bytes would produce a
        // pointer that fails digest check on download. Bail out if absent.
        let digest = self.encryption_iv.clone()?;
        Some(crate::proto::content_v1::AttachmentPointer {
            cdn_key,
            encryption_key,
            digest,
            plaintext_size: self.metadata.size,
            content_type: self.metadata.mime_type.clone(),
            file_name: self.metadata.name.clone(),
            thumbnail_cdn_key: self.metadata.thumbnail_cdn_key.clone().unwrap_or_default(),
            thumbnail_key: self.metadata.thumbnail_key.clone().unwrap_or_default(),
            thumbnail_digest: self.metadata.thumbnail_digest.clone().unwrap_or_default(),
            thumbnail_plaintext_size: self.metadata.thumbnail_plaintext_size.unwrap_or(0),
            thumbnail_width: self.metadata.thumbnail_width.unwrap_or(0),
            thumbnail_height: self.metadata.thumbnail_height.unwrap_or(0),
            width: self.metadata.width.unwrap_or(0),
            height: self.metadata.height.unwrap_or(0),
            duration_ms: self
                .metadata
                .duration_seconds
                .map(|d| (d * 1000.0) as u64)
                .unwrap_or(0),
            // Without these every message-path send (DM, group, resend) shipped
            // a voice note with no bars, and the receiver drew a flat line.
            waveform: self.metadata.waveform_samples.clone().unwrap_or_default(),
            voice_note: SharedCategory::of(&self.metadata) == SharedCategory::Voice,
            ..Default::default()
        })
    }
}

/// File metadata information (matches proto FileMetadata).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    /// SHA-256 hash of original (unencrypted) file
    pub sha256: Vec<u8>,
    /// Chunk size for resumable uploads (0 = not chunked)
    pub chunk_size: u32,
    /// Image/video dimensions
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Audio/video duration in seconds
    pub duration_seconds: Option<f64>,
    /// Audio waveform samples (32 values, 0-100) for visualization
    #[serde(default)]
    pub waveform_samples: Option<Vec<u8>>,
    /// Thumbnail JPEG bytes (sender-provided for instant preview)
    #[serde(default)]
    pub thumbnail: Option<Vec<u8>>,
    /// Thumbnail dimensions
    #[serde(default)]
    pub thumbnail_width: Option<u32>,
    #[serde(default)]
    pub thumbnail_height: Option<u32>,
    /// Thumbnail sidecar — the thumbnail is uploaded as its own small encrypted
    /// CDN object (not inlined in the message) so the attachment pointer fits the
    /// sealed-DM padding budget on every transport. These carry the decryption
    /// material for that sidecar blob; `thumbnail` above holds the decrypted
    /// bytes once fetched (sender fills it immediately, receiver lazily).
    #[serde(default)]
    pub thumbnail_cdn_key: Option<String>,
    #[serde(default)]
    pub thumbnail_key: Option<Vec<u8>>,
    #[serde(default)]
    pub thumbnail_digest: Option<Vec<u8>>,
    #[serde(default)]
    pub thumbnail_plaintext_size: Option<u32>,
}

impl FileMetadata {
    /// Create metadata for a local file.
    #[must_use]
    pub fn new(name: String, mime_type: String, size: u64) -> Self {
        Self {
            name,
            mime_type,
            size,
            sha256: Vec::new(),
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
            thumbnail_cdn_key: None,
            thumbnail_key: None,
            thumbnail_digest: None,
            thumbnail_plaintext_size: None,
        }
    }

    /// One-line type label for a conversation preview — "🎤 Voice message",
    /// "🖼️ Photo", "📄 report.pdf". Media gets a kind word rather than a
    /// filename because "IMG_4821.HEIC" tells the reader nothing a 🖼️ hasn't;
    /// a document keeps its name because that is what identifies it.
    #[must_use]
    pub fn preview_label(&self) -> String {
        let category = FileCategory::from_mime_and_filename(&self.mime_type, &self.name);
        let icon = crate::ui::file_data::category_icon(category.as_str());
        match SharedCategory::of(self) {
            SharedCategory::Voice => "\u{1F3A4} Voice message".to_string(),
            SharedCategory::Gif => "\u{1F39E}\u{FE0F} GIF".to_string(),
            SharedCategory::Music => format!("{} Audio", icon),
            _ => match category {
                FileCategory::Image => format!("{} Photo", icon),
                FileCategory::Video => format!("{} Video", icon),
                _ if self.name.trim().is_empty() => format!("{} File", icon),
                _ => format!("{} {}", icon, self.name.trim()),
            },
        }
    }

    /// Get file category based on MIME type.
    #[must_use]
    pub fn category(&self) -> FileCategory {
        FileCategory::from_mime_type(&self.mime_type)
    }

    /// Check if file is an image.
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(self.category(), FileCategory::Image)
    }

    /// Check if file is a video.
    #[must_use]
    pub fn is_video(&self) -> bool {
        matches!(self.category(), FileCategory::Video)
    }

    /// Check if file is a GIF (by MIME or `.gif` extension).
    #[must_use]
    pub fn is_gif(&self) -> bool {
        self.mime_type == "image/gif" || self.name.to_lowercase().ends_with(".gif")
    }

    /// Check if file is audio.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        matches!(self.category(), FileCategory::Audio)
    }

    /// Check if file is code/text (syntax-highlightable).
    /// Uses both MIME type and filename extension for detection.
    #[must_use]
    pub fn is_code(&self) -> bool {
        matches!(
            FileCategory::from_mime_and_filename(&self.mime_type, &self.name),
            FileCategory::Code
        )
    }

    /// Check if file is a PDF document.
    #[must_use]
    pub fn is_pdf(&self) -> bool {
        self.mime_type == "application/pdf"
    }

    /// Check if file is a document (PDF, Word, etc).
    #[must_use]
    pub fn is_document(&self) -> bool {
        matches!(self.category(), FileCategory::Document)
    }

    /// Check if a file is code/text based on MIME type and filename.
    /// Static version for use when you don't have a full FileMetadata instance.
    #[must_use]
    pub fn is_code_file(filename: &str, mime_type: &str) -> bool {
        matches!(
            FileCategory::from_mime_and_filename(mime_type, filename),
            FileCategory::Code
        )
    }

    /// Check if a file is an image based on MIME type and filename.
    /// Static version for use when you don't have a full FileMetadata instance.
    #[must_use]
    pub fn is_image_file(filename: &str, mime_type: &str) -> bool {
        matches!(
            FileCategory::from_mime_and_filename(mime_type, filename),
            FileCategory::Image
        )
    }

    /// Human-readable file size.
    #[must_use]
    pub fn formatted_size(&self) -> String {
        format_bytes(self.size)
    }
}

/// Human-readable byte count ("2.5 MB").
#[must_use]
pub fn format_bytes(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if size >= GB {
        format!("{:.1} GB", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.1} MB", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.1} KB", size as f64 / KB as f64)
    } else {
        format!("{} bytes", size)
    }
}

/// GIF attachment metadata (from Tenor/Giphy).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GifMetadata {
    pub id: String,
    pub provider: GifProvider,
    pub title: Option<String>,
    /// URL to the GIF file
    pub url: String,
    /// URL to smaller preview
    pub preview_url: String,
    pub width: u32,
    pub height: u32,
}

/// GIF provider source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GifProvider {
    Tenor,
    Giphy,
}

/// Thumbnail cache entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
    pub attachment_id: AttachmentId,
    pub local_path: String,
    pub width: u32,
    pub height: u32,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod preview_label_tests {
    use super::FileMetadata;

    #[test]
    fn every_attachment_kind_gets_a_type_label() {
        let label = |name: &str, mime: &str, waveform: Option<Vec<u8>>| {
            let mut meta = FileMetadata::new(name.into(), mime.into(), 1);
            meta.waveform_samples = waveform;
            meta.preview_label()
        };
        assert_eq!(
            label("Voice message.wav", "audio/wav", Some(vec![13; 32])),
            "\u{1F3A4} Voice message"
        );
        assert_eq!(label("song.mp3", "audio/mpeg", None), "\u{1F3B5} Audio");
        assert_eq!(
            label("IMG_1.HEIC", "image/heic", None),
            "\u{1F5BC}\u{FE0F} Photo"
        );
        assert_eq!(label("a.gif", "image/gif", None), "\u{1F39E}\u{FE0F} GIF");
        assert_eq!(label("clip.mp4", "video/mp4", None), "\u{1F3AC} Video");
        assert_eq!(
            label("report.pdf", "application/pdf", None),
            "\u{1F4C4} report.pdf"
        );
        assert_eq!(
            label(" ", "application/octet-stream", None),
            "\u{1F4CE} File"
        );
    }
}
