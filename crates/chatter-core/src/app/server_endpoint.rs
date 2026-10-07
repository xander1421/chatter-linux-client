//! Resolve one endpoint for startup and both authentication screens.

const LOCAL_SERVER_URL: &str = "https://localhost:50051";

// Android processes do not inherit the build shell's environment. Preserve
// the existing Android override and support the shared endpoint in all builds.
const BUILT_SERVER_URL: Option<&str> = build_server_url(
    if cfg!(target_os = "android") {
        option_env!("CHATTER_ANDROID_SERVER")
    } else {
        None
    },
    option_env!("CHATTER_SERVER_URL"),
);

const fn build_server_url<'a>(
    android: Option<&'a str>,
    shared: Option<&'a str>,
) -> Option<&'a str> {
    match android {
        Some(url) if !url.is_empty() => Some(url),
        _ => match shared {
            Some(url) if !url.is_empty() => Some(url),
            _ => None,
        },
    }
}

// A production mobile artifact must have a usable default before the user
// reaches the authentication screen. Explicit runtime overrides still win.
#[cfg(all(
    target_os = "android",
    not(test),
    not(feature = "dev-trust-root"),
    not(feature = "insecure-test-anchors")
))]
const _: () = assert!(
    BUILT_SERVER_URL.is_some(),
    "Android production builds require CHATTER_SERVER_URL or CHATTER_ANDROID_SERVER at build time"
);

pub(crate) fn resolve_server_url(explicit: Option<String>) -> String {
    select_server_url(
        explicit,
        std::env::var("CHATTER_SERVER_URL").ok(),
        BUILT_SERVER_URL,
    )
}

fn select_server_url(
    explicit: Option<String>,
    runtime: Option<String>,
    built: Option<&str>,
) -> String {
    explicit
        .or(runtime)
        .or_else(|| built.map(str::to_owned))
        .unwrap_or_else(|| LOCAL_SERVER_URL.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_endpoint_wins_over_runtime_and_build() {
        assert_eq!(
            select_server_url(
                Some("https://explicit.invalid".into()),
                Some("https://runtime.invalid".into()),
                Some("https://built.invalid")
            ),
            "https://explicit.invalid"
        );
    }

    #[test]
    fn runtime_endpoint_wins_over_build() {
        assert_eq!(
            select_server_url(
                None,
                Some("https://runtime.invalid".into()),
                Some("https://built.invalid")
            ),
            "https://runtime.invalid"
        );
    }

    #[test]
    fn build_endpoint_reaches_envless_startup() {
        assert_eq!(
            select_server_url(None, None, Some("https://built.invalid")),
            "https://built.invalid"
        );
    }

    #[test]
    fn development_can_default_to_localhost() {
        assert_eq!(select_server_url(None, None, None), LOCAL_SERVER_URL);
    }

    #[test]
    fn android_build_override_wins_over_shared_endpoint() {
        assert_eq!(
            build_server_url(
                Some("https://android.invalid"),
                Some("https://shared.invalid")
            ),
            Some("https://android.invalid")
        );
    }

    #[test]
    fn empty_build_values_do_not_mask_shared_endpoint_or_missing_configuration() {
        assert_eq!(
            build_server_url(Some(""), Some("https://shared.invalid")),
            Some("https://shared.invalid")
        );
        assert_eq!(build_server_url(None, Some("")), None);
        assert_eq!(build_server_url(Some(""), None), None);
    }
}
