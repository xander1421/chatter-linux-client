//! Local display formatting for rows rendered by the client: timestamps,
//! presence, call outcomes, file sizes and attachment categories.

use crate::domain::attachment::format_bytes;
use crate::domain::call::CallResult;
use crate::domain::user::PresenceStatus;
use crate::ui::SharedContentItemData;
use chrono::Duration;
use chrono::{DateTime, Datelike, Local, Utc};

pub fn format_timestamp(datetime: DateTime<Utc>) -> String {
    let local = datetime.with_timezone(&Local);
    let now = Local::now();
    let diff = now.signed_duration_since(local);
    if diff < Duration::minutes(1) {
        return "Just now".to_string();
    }
    if diff < Duration::hours(1) {
        return format!("{} min ago", diff.num_minutes());
    }
    let today = now.date_naive();
    let msg_date = local.date_naive();
    if msg_date == today {
        return local.format("%H:%M").to_string();
    }
    if msg_date == today - Duration::days(1) {
        return format!("Yesterday {}", local.format("%H:%M"));
    }
    if diff < Duration::days(7) {
        return local.format("%a %H:%M").to_string();
    }
    if local.year() == now.year() {
        return local.format("%b %-d").to_string();
    }
    local.format("%b %-d, %Y").to_string()
}

/// A linked device's last activity, to the day. The server records it per
/// UTC day (`user_devices.last_active`, like Signal's `lastSeen`), so the
/// day is compared in UTC: a local date or a time of day would claim more
/// than the server knows.
pub fn format_activity_day(datetime: DateTime<Utc>) -> String {
    let day = datetime.date_naive();
    let today = Utc::now().date_naive();
    if day >= today {
        "Today".to_string()
    } else if day == today - Duration::days(1) {
        "Yesterday".to_string()
    } else if day.year() == today.year() {
        day.format("%b %-d").to_string()
    } else {
        day.format("%b %-d, %Y").to_string()
    }
}

pub fn last_seen_label(presence: PresenceStatus, last_seen_at: Option<i64>) -> String {
    match presence {
        PresenceStatus::Online => "online".to_string(),
        PresenceStatus::Away => "away".to_string(),
        PresenceStatus::DoNotDisturb => "do not disturb".to_string(),
        PresenceStatus::Offline => match last_seen_at {
            Some(secs) => match DateTime::<Utc>::from_timestamp(secs, 0) {
                Some(dt) => format!("last seen {}", format_timestamp(dt).to_lowercase()),
                None => "last seen recently".to_string(),
            },
            None => "last seen recently".to_string(),
        },
    }
}

pub fn shared_content_lines(row: &SharedContentItemData) -> (String, String, bool) {
    let when = format_timestamp(DateTime::from_timestamp_nanos(row.sent_at as i64));
    let is_link = row.category == "link";
    let (title, subtitle) = if is_link {
        (link_host(&row.source_title), row.source_title.clone())
    } else {
        let name = if row.source_title.is_empty() {
            "Attachment".to_string()
        } else {
            row.source_title.clone()
        };
        let size = if row.size_bytes > 0 {
            format!("{} · {}", format_bytes(row.size_bytes), when)
        } else {
            when
        };
        (name, size)
    };
    let is_video = row.is_video;
    (title, subtitle, is_video)
}


fn link_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
}

/// The chat list's short local date, shared with call history.
pub fn sidebar_time(at: DateTime<Utc>) -> String {
    crate::services::server_records::sidebar_time_at(at, Local::now())
}

pub fn call_detail(
    result: Option<CallResult>,
    incoming: bool,
    duration_secs: Option<i64>,
) -> String {
    let outcome = match result {
        Some(CallResult::Missed) if incoming => "Missed",
        Some(CallResult::Missed) => "No answer",
        Some(CallResult::Declined) => "Declined",
        Some(CallResult::Failed) => "Failed",
        Some(CallResult::Completed) if incoming => "Incoming",
        Some(CallResult::Completed) => "Outgoing",
        None => "",
    };
    match duration_secs {
        Some(secs) if secs > 0 && result == Some(CallResult::Completed) => {
            format!("{} \u{00B7} {}", outcome, format_call_duration(secs))
        }
        _ => outcome.to_string(),
    }
}

pub fn format_call_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// Full local date and time for server record details.
pub fn server_record_time(nanos: u64) -> String {
    DateTime::from_timestamp_nanos(nanos as i64)
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Format file size in human-readable form.
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Map MIME type to category for UI display.
pub fn mime_to_category(mime_type: &str) -> &'static str {
    if mime_type.starts_with("image/") {
        "image"
    } else if mime_type.starts_with("video/") {
        "video"
    } else if mime_type.starts_with("audio/") {
        "audio"
    } else if mime_type.starts_with("text/") || mime_type == "application/json" {
        "code"
    } else if mime_type == "application/pdf"
        || mime_type.starts_with("application/msword")
        || mime_type.contains("document")
    {
        "document"
    } else if mime_type == "application/zip"
        || mime_type == "application/x-rar-compressed"
        || mime_type == "application/x-tar"
        || mime_type == "application/gzip"
    {
        "archive"
    } else {
        "other"
    }
}

/// Convert stored/wire amplitudes (0-255) to the UI's percentage bar scale.
pub fn waveform_bar(sample: i32) -> i32 {
    (sample.clamp(0, 255) * 100 + 127) / 255
}
