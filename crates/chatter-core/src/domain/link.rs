//! Decision for opening links supplied by another user.

/// Return the real destination to show before opening a disguised or IDN link.
pub fn link_needs_confirmation(display_text: &str, target_url: &str) -> Option<String> {
    let target = url::Url::parse(target_url).ok()?;
    let shown = target.to_string();
    let idn = target
        .host_str()
        .is_some_and(|host| host.split('.').any(|label| label.starts_with("xn--")));
    let matches_target = url::Url::parse(display_text)
        .is_ok_and(|display| display.to_string() == shown);
    (!matches_target || idn).then_some(shown)
}

#[cfg(test)]
mod tests {
    use super::link_needs_confirmation;

    #[test]
    fn confirms_disguised_and_idn_links() {
        assert_eq!(
            link_needs_confirmation("https://mybank.example/login", "https://evil.example"),
            Some("https://evil.example/".into())
        );
        assert_eq!(link_needs_confirmation("https://example.org", "https://example.org"), None);
        assert_eq!(
            link_needs_confirmation("https://аpple.com", "https://аpple.com"),
            Some("https://xn--pple-43d.com/".into())
        );
    }
}
