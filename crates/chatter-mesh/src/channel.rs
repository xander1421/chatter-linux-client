//! Chatter open BLE channel payload (extension type 0xE3).
//!
//! Version 1: `[version=1][name_len:u8][canonical ASCII name][UTF-8 text]`.
//! The packet header's sender is the author, and its signature covers this
//! complete payload. Public channel names and bodies are visible to every relay.

pub const MAX_CHANNEL_NAME: usize = 32;
pub const MAX_CHANNEL_TEXT: usize = 256;

/// Lowercase ASCII slug, without a leading `#`.
pub fn canonical_name(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let name = trimmed
        .strip_prefix('#')
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    if name.is_empty() || name.len() > MAX_CHANNEL_NAME {
        return None;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return None;
    }
    Some(name)
}

pub fn encode(name: &str, text: &str) -> Option<Vec<u8>> {
    let name = canonical_name(name)?;
    if text.trim().is_empty() || text.len() > MAX_CHANNEL_TEXT {
        return None;
    }
    let mut out = Vec::with_capacity(2 + name.len() + text.len());
    out.push(1);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

/// Version 2 advertises an active channel without publishing a message.
pub fn encode_presence(name: &str) -> Option<Vec<u8>> {
    let name = canonical_name(name)?;
    let mut out = Vec::with_capacity(2 + name.len());
    out.push(2);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    Some(out)
}

pub fn decode_presence(bytes: &[u8]) -> Option<String> {
    let (&version, rest) = bytes.split_first()?;
    if version != 2 {
        return None;
    }
    let (&len, rest) = rest.split_first()?;
    if rest.len() != len as usize {
        return None;
    }
    let name = std::str::from_utf8(rest).ok()?;
    canonical_name(name).filter(|canonical| canonical == name)
}

pub fn decode(bytes: &[u8]) -> Option<(String, String)> {
    let (&version, rest) = bytes.split_first()?;
    if version != 1 {
        return None;
    }
    let (&name_len, rest) = rest.split_first()?;
    let (name, body) = rest.split_at_checked(name_len as usize)?;
    let name = std::str::from_utf8(name).ok()?;
    let name = canonical_name(name).filter(|canonical| canonical == name)?;
    if body.is_empty() || body.len() > MAX_CHANNEL_TEXT {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?.to_string();
    if text.trim().is_empty() {
        return None;
    }
    Some((name, text))
}
