//! Chatter's translations.
//!
//! English is the source language and lives in the UI (`ui/*.slint`, marked
//! with `@tr("…")`). Every other language lives here as a gettext catalog,
//! `translations/<code>/LC_MESSAGES/chatter-ui-slint.po`, which
//! `chatter-ui-slint`'s build script bundles into the generated UI
//! (`slint_build::CompilerConfiguration::with_bundled_translations`). The UI
//! switches language with `slint::select_bundled_translation(code)`.
//!
//! This crate owns the list of languages and the rule that maps a system
//! locale to one of them. Adding a language = one `.po` file plus one
//! [`LANGUAGES`] row.

mod runtime;

pub use runtime::{set_language, tr, trc, trf, trfc, trn};

/// A language the app ships.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language {
    /// Catalog directory and the code given to
    /// `slint::select_bundled_translation`. `"en"` has no catalog.
    pub code: &'static str,
    /// The language's own name, as shown in the language picker.
    pub native_name: &'static str,
    /// Its English name, for the picker's second line and for search.
    pub english_name: &'static str,
}

/// The source language. It has no catalog: untranslated text is English.
pub const SOURCE: Language = Language { code: "en", native_name: "English", english_name: "English" };

/// Every language the app offers, source language first.
pub const LANGUAGES: &[Language] = &[
    SOURCE,
    Language { code: "de", native_name: "Deutsch", english_name: "German" },
    Language { code: "es", native_name: "Español", english_name: "Spanish" },
    Language { code: "fr", native_name: "Français", english_name: "French" },
    Language { code: "it", native_name: "Italiano", english_name: "Italian" },
    Language { code: "pt", native_name: "Português", english_name: "Portuguese" },
];

/// Directory holding the catalogs, for `chatter-ui-slint`'s build script.
pub const TRANSLATIONS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/translations");

/// The shipped language with this exact code.
#[must_use]
pub fn language(code: &str) -> Option<Language> {
    LANGUAGES.iter().copied().find(|language| language.code == code)
}

/// The shipped language for a system locale such as `pt_BR.UTF-8`, `ro-RO` or
/// `uk`; English when the locale's language is not shipped.
#[must_use]
pub fn language_for_locale(locale: &str) -> Language {
    let primary = locale
        .split(['_', '-', '.', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    language(&primary).unwrap_or(SOURCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_maps_to_a_shipped_language_or_english() {
        for language in LANGUAGES {
            let code = language.code;
            assert_eq!(language_for_locale(&format!("{code}_XX.UTF-8")).code, code);
            assert_eq!(language_for_locale(&format!("{code}-XX")).code, code);
            assert_eq!(language_for_locale(&code.to_uppercase()).code, code);
        }
        assert_eq!(language_for_locale("ja_JP").code, "en");
        assert_eq!(language_for_locale("").code, "en");
        assert_eq!(language_for_locale("C").code, "en");
    }

    #[test]
    fn every_language_but_the_source_has_a_catalog() {
        for language in LANGUAGES.iter().filter(|language| **language != SOURCE) {
            let catalog = std::path::Path::new(TRANSLATIONS_DIR)
                .join(language.code)
                .join("LC_MESSAGES/chatter-ui-slint.po");
            assert!(catalog.is_file(), "missing {}", catalog.display());
        }
    }
}
