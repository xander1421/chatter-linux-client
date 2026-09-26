//! Appearance preferences — palette, text size and message density.
//!
//! Device-local (the settings DB, not the server): a theme is a property of
//! the screen in front of you, not of the account, exactly as in Signal.

/// Settings › Appearance › Theme. Persisted under `keys::THEME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// Follow the OS light/dark setting (and its changes, live).
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    /// Parse the persisted value. Unknown/missing → [`ThemeMode::System`].
    pub fn from_str(s: &str) -> Self {
        match s {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            _ => ThemeMode::System,
        }
    }

    /// The persisted string form — also the value the Slint `Appearance`
    /// global takes.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

/// Settings › Appearance › Text size. Persisted under `keys::TEXT_SIZE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextSize {
    Small,
    /// Follow the OS font size (Android's Settings › Display › Font size;
    /// desktop has none, so 1.0).
    #[default]
    Default,
    Large,
    Largest,
}

impl TextSize {
    const SMALL: f32 = 0.9;
    const LARGE: f32 = 1.15;
    /// The ceiling the layout was audited against: nothing may draw text
    /// larger than this, or fixed-height rows start to clip.
    const LARGEST: f32 = 1.3;

    /// Parse the persisted value. Unknown/missing → [`TextSize::Default`].
    pub fn from_str(s: &str) -> Self {
        match s {
            "small" => TextSize::Small,
            "large" => TextSize::Large,
            "largest" => TextSize::Largest,
            _ => TextSize::Default,
        }
    }

    /// The persisted string form — also the value the Slint `Appearance`
    /// global takes.
    pub fn as_str(self) -> &'static str {
        match self {
            TextSize::Small => "small",
            TextSize::Default => "default",
            TextSize::Large => "large",
            TextSize::Largest => "largest",
        }
    }

    /// The multiplier every `Typography` size is drawn at. `system` is the
    /// OS font scale; "Default" follows it, clamped to the span of the fixed
    /// steps (Android 14 goes to 2.0, which no row in this app was laid out for).
    pub fn scale(self, system: f32) -> f32 {
        match self {
            TextSize::Small => Self::SMALL,
            TextSize::Default if system.is_finite() => system.clamp(Self::SMALL, Self::LARGEST),
            TextSize::Default => 1.0,
            TextSize::Large => Self::LARGE,
            TextSize::Largest => Self::LARGEST,
        }
    }
}

/// Settings › Appearance › Message spacing. Persisted under
/// `keys::MESSAGE_DENSITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageDensity {
    #[default]
    Comfortable,
    Compact,
}

impl MessageDensity {
    /// Parse the persisted value. Unknown/missing → [`MessageDensity::Comfortable`].
    pub fn from_str(s: &str) -> Self {
        match s {
            "compact" => MessageDensity::Compact,
            _ => MessageDensity::Comfortable,
        }
    }

    /// The persisted string form — also the value the Slint `Appearance`
    /// global takes.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageDensity::Comfortable => "comfortable",
            MessageDensity::Compact => "compact",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_to_system() {
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(ThemeMode::from_str(mode.as_str()), mode);
        }
        assert_eq!(ThemeMode::from_str(""), ThemeMode::System);
        assert_eq!(ThemeMode::from_str("sepia"), ThemeMode::System);
    }

    #[test]
    fn text_size_round_trips_and_default_follows_the_os_within_bounds() {
        for size in [TextSize::Small, TextSize::Default, TextSize::Large, TextSize::Largest] {
            assert_eq!(TextSize::from_str(size.as_str()), size);
        }
        assert_eq!(TextSize::from_str(""), TextSize::Default);
        assert_eq!(TextSize::Default.scale(1.1), 1.1);
        // Android 14's 200% must not escape the audited ceiling, nor a
        // tiny OS scale go below the smallest step.
        assert_eq!(TextSize::Default.scale(2.0), TextSize::Largest.scale(1.0));
        assert_eq!(TextSize::Default.scale(0.5), TextSize::Small.scale(1.0));
        assert_eq!(TextSize::Default.scale(f32::NAN), 1.0);
        // An explicit pick ignores the OS.
        assert_eq!(TextSize::Large.scale(2.0), TextSize::Large.scale(0.5));
        assert!(TextSize::Small.scale(1.0) < 1.0 && TextSize::Largest.scale(1.0) > TextSize::Large.scale(1.0));
    }

    #[test]
    fn density_round_trips_and_defaults_to_comfortable() {
        for d in [MessageDensity::Comfortable, MessageDensity::Compact] {
            assert_eq!(MessageDensity::from_str(d.as_str()), d);
        }
        assert_eq!(MessageDensity::from_str("dense"), MessageDensity::Comfortable);
    }
}
