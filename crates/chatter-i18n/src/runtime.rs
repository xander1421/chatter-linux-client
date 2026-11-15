//! Translating text that Rust produces (toasts, errors, notification text).
//!
//! The UI's own text is translated by Slint from `chatter-ui-slint.po`. Text
//! built in Rust goes through [`tr`], [`trf`] and [`trn`], which look the
//! English source up in `translations/<code>/LC_MESSAGES/chatter-rust.po` for
//! the language set with [`set_language`]. A missing entry is the English
//! text, so an untranslated string is never an error.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::LANGUAGES;

macro_rules! catalog {
    ($code:literal) => {
        include_str!(concat!("../translations/", $code, "/LC_MESSAGES/chatter-rust.po"))
    };
}

/// Catalog text per [`LANGUAGES`] row; the source language has none.
const CATALOGS: &[&str] = &[
    "",
    catalog!("de"),
    catalog!("es"),
];

const _: () = assert!(CATALOGS.len() == LANGUAGES.len(), "one catalog per LANGUAGES row");

/// Index into [`LANGUAGES`] of the language in force.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Use this shipped language for [`tr`] from now on; an unknown code is English.
pub fn set_language(code: &str) {
    let index = LANGUAGES.iter().position(|language| language.code == code).unwrap_or(0);
    CURRENT.store(index, Ordering::Relaxed);
}

/// msgid → msgstr forms (one, or one per plural class).
type Catalog = HashMap<String, Vec<String>>;

fn catalog(index: usize) -> &'static Catalog {
    static PARSED: [OnceLock<Catalog>; CATALOGS.len()] = [const { OnceLock::new() }; CATALOGS.len()];
    PARSED[index].get_or_init(|| parse(CATALOGS[index]))
}

/// Catalog key of `text` in `context` (gettext's own separator); bare text without one.
fn key(context: &str, text: &str) -> String {
    if context.is_empty() { text.to_owned() } else { format!("{context}\u{4}{text}") }
}

/// The text between a line's first and last quote, unescaped.
fn quoted(line: &str) -> String {
    let inner = line.find('"').zip(line.rfind('"')).map_or("", |(a, b)| if b > a { &line[a + 1..b] } else { "" });
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn parse(po: &str) -> Catalog {
    #[derive(Clone, Copy)]
    enum Field {
        None,
        Id,
        Skip,
        Str(usize),
    }
    let mut catalog = Catalog::new();
    let (mut id, mut forms, mut field) = (String::new(), Vec::<String>::new(), Field::None);
    let mut context = String::new();
    let mut flush = |id: &mut String, forms: &mut Vec<String>| {
        if !id.is_empty() && forms.iter().all(|form| !form.is_empty()) && !forms.is_empty() {
            catalog.insert(std::mem::take(id), std::mem::take(forms));
        }
        id.clear();
        forms.clear();
    };
    for line in po.lines().map(str::trim) {
        if line.starts_with("msgctxt ") {
            flush(&mut id, &mut forms);
            context = quoted(line);
            field = Field::None;
        } else if line.starts_with("msgid ") {
            flush(&mut id, &mut forms);
            id = key(&std::mem::take(&mut context), &quoted(line));
            field = Field::Id;
        } else if line.starts_with("msgid_plural ") {
            field = Field::Skip;
        } else if line.starts_with("msgstr") {
            let index = line[6..].strip_prefix('[').and_then(|rest| rest.split(']').next()?.parse().ok()).unwrap_or(0);
            forms.resize(forms.len().max(index + 1), String::new());
            forms[index] = quoted(line);
            field = Field::Str(index);
        } else if line.starts_with('"') {
            match field {
                Field::Id => id.push_str(&quoted(line)),
                Field::Str(index) => forms[index].push_str(&quoted(line)),
                Field::Skip | Field::None => {}
            }
        } else {
            field = Field::None;
        }
    }
    flush(&mut id, &mut forms);
    catalog
}

/// Which plural form `n` takes in the language `code` — the same rules as the
/// catalogs' `Plural-Forms` headers.
fn plural_class(code: &str, n: u64) -> usize {
    match code {
        "fr" => usize::from(n > 1),
        "ro" => {
            if n == 1 {
                0
            } else if n == 0 || (1..20).contains(&(n % 100)) {
                1
            } else {
                2
            }
        }
        "ru" | "uk" => {
            if n % 10 == 1 && n % 100 != 11 {
                0
            } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
                1
            } else {
                2
            }
        }
        _ => usize::from(n != 1),
    }
}

/// `text` in the language in force; `text` itself when there is no translation.
#[must_use]
pub fn tr(text: &str) -> String {
    let index = CURRENT.load(Ordering::Relaxed);
    if index == 0 {
        return text.to_owned();
    }
    catalog(index).get(text).and_then(|forms| forms.first()).map_or_else(|| text.to_owned(), Clone::clone)
}

/// [`tr`] for English text that means different things in different places:
/// `context` names this use ("month abbreviation") and selects its own entry.
#[must_use]
pub fn trc(context: &str, text: &str) -> String {
    let index = CURRENT.load(Ordering::Relaxed);
    if index == 0 {
        return text.to_owned();
    }
    catalog(index).get(&key(context, text)).and_then(|forms| forms.first()).map_or_else(|| text.to_owned(), Clone::clone)
}

/// [`trc`], then placeholders filled as in [`trf`].
#[must_use]
pub fn trfc(context: &str, text: &str, args: &[&dyn Display]) -> String {
    fill(&trc(context, text), args)
}

/// [`tr`], then each `{}` replaced in order by `args` (and `{0}`, `{1}` by position).
#[must_use]
pub fn trf(text: &str, args: &[&dyn Display]) -> String {
    fill(&tr(text), args)
}

/// The singular or plural text for `n` in the language in force, with `{n}`
/// replaced by `n` and other placeholders by `args`.
#[must_use]
pub fn trn(singular: &str, plural: &str, n: u64, args: &[&dyn Display]) -> String {
    let index = CURRENT.load(Ordering::Relaxed);
    let english = if n == 1 { singular } else { plural };
    let text = if index == 0 {
        english
    } else {
        catalog(index)
            .get(singular)
            .and_then(|forms| forms.get(plural_class(LANGUAGES[index].code, n)))
            .map_or(english, String::as_str)
    };
    fill(&text.replace("{n}", &n.to_string()), args)
}

fn fill(text: &str, args: &[&dyn Display]) -> String {
    let mut out = text.to_owned();
    for (position, arg) in args.iter().enumerate() {
        let value = arg.to_string();
        out = out.replace(&format!("{{{position}}}"), &value);
        if let Some(at) = out.find("{}") {
            out.replace_range(at..at + 2, &value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_multiline_and_plural_entries() {
        let catalog = parse(
            "msgid \"\"\nmsgstr \"header\"\n\n#: a.rs:1\nmsgid \"Hi {}\"\nmsgstr \"Salut \\\"{}\\\"\"\n\n\
             msgid \"\"\n\"Long \"\n\"text\"\nmsgstr \"\"\n\"Text \"\n\"lung\"\n\n\
             msgid \"{n} file\"\nmsgid_plural \"{n} files\"\nmsgstr[0] \"un fișier\"\nmsgstr[1] \"{n} fișiere\"\n\
             msgstr[2] \"{n} de fișiere\"\n\nmsgid \"Empty\"\nmsgstr \"\"\n",
        );
        assert_eq!(catalog["Hi {}"], ["Salut \"{}\""]);
        assert_eq!(catalog["Long text"], ["Text lung"]);
        assert_eq!(catalog["{n} file"].len(), 3);
        assert!(!catalog.contains_key("Empty"));
        let catalog = parse("msgctxt \"month abbreviation\"\nmsgid \"May\"\nmsgstr \"mai\"\n\nmsgid \"May\"\nmsgstr \"Mai\"\n");
        assert_eq!(catalog[&key("month abbreviation", "May")], ["mai"]);
        assert_eq!(catalog["May"], ["Mai"]);
    }

    #[test]
    fn plural_classes_follow_the_catalog_headers() {
        assert_eq!([1, 2, 5, 11, 21, 22].map(|n| plural_class("ru", n)), [0, 1, 2, 2, 0, 1]);
        assert_eq!([0, 1, 2, 19, 20, 101].map(|n| plural_class("ro", n)), [1, 0, 1, 1, 2, 1]);
        assert_eq!([0, 1, 2].map(|n| plural_class("fr", n)), [0, 0, 1]);
        assert_eq!([0, 1, 2].map(|n| plural_class("de", n)), [1, 0, 1]);
    }

    #[test]
    fn placeholders_fill_in_order_and_by_position() {
        assert_eq!(fill("{} sent {}", &[&"Ana", &3]), "Ana sent 3");
        assert_eq!(fill("{1} of {0}", &[&"a", &"b"]), "b of a");
        // English is the fallback for every lookup.
        assert_eq!(trn("{n} file", "{n} files", 2, &[]), "2 files");
        assert_eq!(tr("Not in any catalog"), "Not in any catalog");
    }
}
