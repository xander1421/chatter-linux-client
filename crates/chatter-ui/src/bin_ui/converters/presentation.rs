//! Local display formatting for rows rendered by the client.

use chatter_core::domain::attachment::{FileCategory, format_bytes};
use chatter_core::domain::call::CallResult;
use chatter_core::domain::user::PresenceStatus;
use chatter_core::ui::SharedContentItemData;
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
    let is_video = shared_content_is_video(row);
    (title, subtitle, is_video)
}

pub fn shared_content_is_video(row: &SharedContentItemData) -> bool {
    matches!(FileCategory::from_filename(&row.source_title), FileCategory::Video)
}

pub fn shared_tab_label(id: &str) -> &'static str {
    match id {
        "media" => "Media",
        "file" => "Files",
        "link" => "Links",
        "voice" => "Voice",
        "gif" => "GIFs",
        "music" => "Music",
        _ => "",
    }
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

pub fn day_label(datetime: DateTime<Utc>) -> String {
    day_label_at(
        datetime.with_timezone(&Local).date_naive(),
        Local::now().date_naive(),
    )
}

pub fn initials_from_name(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .collect::<String>()
        .to_uppercase()
}

fn day_label_at(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    let days_ago = (today - day).num_days();
    match days_ago {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%b %-d").to_string(),
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

/// The chat list's short local date, shared with call history.
pub fn sidebar_time(at: DateTime<Utc>) -> String {
    sidebar_time_at(at, Local::now())
}

pub fn sidebar_time_at(at: DateTime<Utc>, now: DateTime<Local>) -> String {
    let local = at.with_timezone(&Local);
    let days = (now.date_naive() - local.date_naive()).num_days();
    match days {
        ..=0 => local.format("%H:%M").to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => local.format("%a").to_string(),
        _ if local.year() == now.year() => local.format("%b %-d").to_string(),
        _ => local.format("%b %-d, %Y").to_string(),
    }
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
