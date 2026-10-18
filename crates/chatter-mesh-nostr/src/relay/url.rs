//! Relay URL canonicalization and directory addresses.

// --- URL normalisation -----------------------------------------------------

/// Canonicalises a relay URL, or `None` if it is not one.
///
/// Ported from `NostrRelayURL.normalized`. `https`/`wss` collapse to `wss` and
/// `http`/`ws` to `ws`; the host lowercases; the scheme's default port and a bare
/// `"/"` path drop; the fragment is discarded. Canonical form matters beyond
/// tidiness — the URL string is the map key that decides whether we already hold a
/// connection, so `wss://R.example:443/` and `wss://r.example` must not become two
/// sockets to the same relay.
///
/// Two deliberate tightenings over `URLComponents`:
///
/// * An authority carrying userinfo (`user:pass@host`) is rejected. Relay URLs
///   arrive from a downloaded CSV and from user input; credentials in one are
///   either a mistake or an attempt to smuggle something past the host check.
/// * Hosts are ASCII. `URLComponents` would percent-encode or punycode a
///   non-ASCII host; producing a different byte string than bitchat for the same
///   input is worse than refusing it, and no relay uses one.
pub fn normalize_relay_url(raw: &str, default_scheme: Option<&str>) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let with_scheme;
    let value = if trimmed.contains("://") {
        trimmed
    } else {
        with_scheme = format!("{}://{trimmed}", default_scheme?);
        with_scheme.as_str()
    };

    let (raw_scheme, rest) = value.split_once("://")?;
    let (scheme, default_port) = match raw_scheme.to_ascii_lowercase().as_str() {
        "wss" | "https" => ("wss", 443u16),
        "ws" | "http" => ("ws", 80u16),
        _ => return None,
    };

    // The fragment never reaches the wire.
    let rest = match rest.split_once('#') {
        Some((head, _)) => head,
        None => rest,
    };

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end)?;
    let tail = rest.get(authority_end..)?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    let (raw_host, raw_port) = split_host_port(authority)?;
    let host = raw_host.to_ascii_lowercase();
    if !is_valid_host(&host) {
        return None;
    }

    let port = match raw_port {
        // `host:` has no port at all, which is how `URLComponents` reads it too.
        None | Some("") => None,
        Some(digits) => Some(digits.parse::<u16>().ok()?),
    }
    .filter(|port| *port != default_port);

    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    let path = if path == "/" { "" } else { path };

    let mut out = String::with_capacity(value.len() + 8);
    out.push_str(scheme);
    out.push_str("://");
    out.push_str(&host);
    if let Some(port) = port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(path);
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

/// The normalised host (and port, and path) with the scheme stripped — the form
/// the geo relay CSV stores. Ported from `NostrRelayURL.directoryAddress`.
pub fn directory_address(raw: &str) -> Option<String> {
    let normalized = normalize_relay_url(raw, Some("wss"))?;
    for prefix in ["wss://", "ws://"] {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    Some(normalized)
}

/// Splits `host:port`, keeping a bracketed IPv6 literal intact.
fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = authority.get(..=close)?;
        // `]` is one byte, so this index is on a character boundary.
        let after = authority.get(close + 1..)?;
        return match after {
            "" => Some((host, None)),
            other => Some((host, Some(other.strip_prefix(':')?))),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

fn is_valid_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}
