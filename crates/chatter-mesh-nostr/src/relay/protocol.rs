//! NIP-01 filters and relay wire frames.

use serde_json::{Map, Value};

use super::DEFAULT_FETCH_LIMIT;
use crate::error::NostrError;
use crate::types::NostrEvent;

// --- Filters ---------------------------------------------------------------

/// A NIP-01 subscription filter.
///
/// Empty `kinds`/`authors` mean "no constraint" and are omitted from the JSON, so
/// `RelayFilter::default()` serialises to `{}` and matches everything. A tag whose
/// value list is empty is *not* omitted: `{"#g": []}` is a filter that matches
/// nothing, and silently widening it to "any geohash" would subscribe a user to
/// traffic they never asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayFilter {
    pub kinds: Vec<u16>,
    pub authors: Vec<String>,
    /// Tag filters, serialised as `#<name>`. Order is preserved.
    pub tags: Vec<(String, Vec<String>)>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<usize>,
}

impl RelayFilter {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        if !self.kinds.is_empty() {
            map.insert(
                "kinds".to_string(),
                Value::Array(self.kinds.iter().map(|k| Value::from(*k)).collect()),
            );
        }
        if !self.authors.is_empty() {
            map.insert(
                "authors".to_string(),
                Value::Array(
                    self.authors
                        .iter()
                        .map(|a| Value::from(a.as_str()))
                        .collect(),
                ),
            );
        }
        for (name, values) in &self.tags {
            map.insert(
                format!("#{name}"),
                Value::Array(values.iter().map(|v| Value::from(v.as_str())).collect()),
            );
        }
        if let Some(since) = self.since {
            map.insert("since".to_string(), Value::from(since));
        }
        if let Some(until) = self.until {
            map.insert("until".to_string(), Value::from(until));
        }
        if let Some(limit) = self.limit {
            map.insert("limit".to_string(), Value::from(limit));
        }
        Value::Object(map)
    }

    /// NIP-17 gift wraps addressed to us.
    pub fn gift_wraps_for(pubkey: &str, since: Option<i64>) -> Self {
        Self {
            kinds: vec![1059],
            tags: vec![("p".to_string(), vec![pubkey.to_string()])],
            since,
            limit: Some(DEFAULT_FETCH_LIMIT),
            ..Self::default()
        }
    }

    /// Geohash channel traffic: chat (20000) and presence (20001).
    ///
    /// Both kinds are ephemeral, so `since` only bounds what a relay forwards live
    /// — it can never produce history.
    pub fn geohash_ephemeral(geohash: &str, since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![20000, 20001],
            tags: vec![("g".to_string(), vec![geohash.to_string()])],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// Courier drops (kind 1401) parked under rotating `#x` recipient tags.
    pub fn courier_drops(recipient_tags_hex: &[String], since: Option<i64>, limit: usize) -> Self {
        Self {
            kinds: vec![1401],
            tags: vec![("x".to_string(), recipient_tags_hex.to_vec())],
            since,
            limit: Some(limit),
            ..Self::default()
        }
    }
}

// --- Framing ---------------------------------------------------------------

/// A frame from a relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayMessage {
    Event {
        subscription_id: String,
        event: NostrEvent,
    },
    /// End of stored events: everything that follows on this subscription is live.
    Eose {
        subscription_id: String,
    },
    /// NIP-20 publish result. `accepted = false` means the event was **dropped**.
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    Notice(String),
    /// The relay terminated a subscription from its side.
    Closed {
        subscription_id: String,
        message: String,
    },
}

/// What bitchat substitutes when a relay sends an `OK` with no reason.
pub(super) const NO_REASON_GIVEN: &str = "no reason given";

/// Parses one relay frame.
///
/// Everything here runs on bytes chosen by an untrusted relay, so every access is
/// checked and nothing is indexed. A frame we cannot make sense of is an error the
/// caller drops, never a panic and never a partially-applied state change.
pub fn parse_relay_message(raw: &str) -> Result<RelayMessage, NostrError> {
    let mut value: Value = serde_json::from_str(raw).map_err(|e| NostrError::Json(e.to_string()))?;
    let array = value
        .as_array()
        .ok_or_else(|| NostrError::Relay("relay frame is not a JSON array".to_string()))?;

    let kind = array
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| NostrError::Relay("relay frame has no type".to_string()))?
        .to_string();

    match kind.as_str() {
        "EVENT" => {
            let subscription_id = string_at(array, 1, "EVENT subscription id")?;
            let raw_event = value
                .as_array_mut()
                .and_then(|items| items.get_mut(2))
                .ok_or_else(|| NostrError::Relay("EVENT frame has no event".to_string()))?;
            let event: NostrEvent = serde_json::from_value(raw_event.take())
                .map_err(|e| NostrError::Json(e.to_string()))?;
            Ok(RelayMessage::Event {
                subscription_id,
                event,
            })
        }
        "EOSE" => Ok(RelayMessage::Eose {
            subscription_id: string_at(array, 1, "EOSE subscription id")?,
        }),
        "OK" => {
            let event_id = string_at(array, 1, "OK event id")?;
            let accepted = array
                .get(2)
                .and_then(Value::as_bool)
                .ok_or_else(|| NostrError::Relay("OK frame has no accepted flag".to_string()))?;
            let message = array
                .get(3)
                .and_then(Value::as_str)
                .unwrap_or(NO_REASON_GIVEN)
                .to_string();
            Ok(RelayMessage::Ok {
                event_id,
                accepted,
                message,
            })
        }
        "NOTICE" => Ok(RelayMessage::Notice(string_at(array, 1, "NOTICE message")?)),
        "CLOSED" => Ok(RelayMessage::Closed {
            subscription_id: string_at(array, 1, "CLOSED subscription id")?,
            message: array
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        other => Err(NostrError::Relay(format!(
            "unknown relay frame type {}",
            // Truncated: the type is attacker-chosen and this string ends up in a
            // log line.
            other.chars().take(32).collect::<String>()
        ))),
    }
}

fn string_at(array: &[Value], index: usize, what: &'static str) -> Result<String, NostrError> {
    array
        .get(index)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| NostrError::Relay(format!("relay frame is missing {what}")))
}

/// `["REQ", <id>, <filter>, ...]`.
///
/// A relay requires at least one filter; passing none produces a frame it will
/// reject. [`RelayClient::subscribe`] refuses that case up front — this function
/// stays infallible so framing can be tested as pure data.
pub fn subscription_request(id: &str, filters: &[RelayFilter]) -> String {
    let mut frame = vec![Value::from("REQ"), Value::from(id)];
    frame.extend(filters.iter().map(RelayFilter::to_json));
    Value::Array(frame).to_string()
}

/// `["EVENT", <event>]`.
pub fn publish_request(event: &NostrEvent) -> Result<String, NostrError> {
    let value = serde_json::to_value(event).map_err(|e| NostrError::Json(e.to_string()))?;
    Ok(Value::Array(vec![Value::from("EVENT"), value]).to_string())
}

/// `["CLOSE", <id>]`.
pub fn close_request(id: &str) -> String {
    Value::Array(vec![Value::from("CLOSE"), Value::from(id)]).to_string()
}
