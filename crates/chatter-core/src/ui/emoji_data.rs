//! The emoji picker's data: the table, search, skin tones and recents, shared
//! by the composer picker and the reaction picker. The UI renders what these
//! return.
//!
//! Only colour-default emoji are offered (see [`is_color_default`]): a
//! text-default one renders grey, or as tofu on older Android fonts.

/// Category index reserved for "Recent" emoji (displayed as first tab).
pub const RECENT_CATEGORY: i32 = -1;
/// The tab a fresh picker opens on when there is no history.
pub const DEFAULT_CATEGORY: i32 = 0;
/// People & Body: the one category skin tones apply to.
pub const PEOPLE_CATEGORY: i32 = 1;

/// One picker cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerEmoji {
    pub emoji: String,
    pub name: String,
    pub category: i32,
}

/// SMP code points with Emoji_Presentation=No (Unicode `emoji-data.txt`).
/// Being ≥ U+1F000 does not make an emoji colour-default: these are text-style
/// even with VS16 on older fonts, and rendered as tofu on Android 10 (🛡️).
const SMP_TEXT_DEFAULT: &[(u32, u32)] = &[
    (0x1F170, 0x1F171),
    (0x1F17E, 0x1F17F),
    (0x1F202, 0x1F202),
    (0x1F237, 0x1F237),
    (0x1F321, 0x1F321),
    (0x1F324, 0x1F32C),
    (0x1F336, 0x1F336),
    (0x1F37D, 0x1F37D),
    (0x1F396, 0x1F397),
    (0x1F399, 0x1F39B),
    (0x1F39E, 0x1F39F),
    (0x1F3CB, 0x1F3CE),
    (0x1F3D4, 0x1F3DF),
    (0x1F3F3, 0x1F3F3),
    (0x1F3F5, 0x1F3F5),
    (0x1F3F7, 0x1F3F7),
    (0x1F43F, 0x1F43F),
    (0x1F441, 0x1F441),
    (0x1F4FD, 0x1F4FD),
    (0x1F549, 0x1F54A),
    (0x1F56F, 0x1F570),
    (0x1F573, 0x1F579),
    (0x1F587, 0x1F587),
    (0x1F58A, 0x1F58D),
    (0x1F590, 0x1F590),
    (0x1F5A5, 0x1F5A5),
    (0x1F5A8, 0x1F5A8),
    (0x1F5B1, 0x1F5B2),
    (0x1F5BC, 0x1F5BC),
    (0x1F5C2, 0x1F5C4),
    (0x1F5D1, 0x1F5D3),
    (0x1F5DC, 0x1F5DE),
    (0x1F5E1, 0x1F5E1),
    (0x1F5E3, 0x1F5E3),
    (0x1F5E8, 0x1F5E8),
    (0x1F5EF, 0x1F5EF),
    (0x1F5F3, 0x1F5F3),
    (0x1F5FA, 0x1F5FA),
    (0x1F6CB, 0x1F6CB),
    (0x1F6CD, 0x1F6CF),
    (0x1F6E0, 0x1F6E5),
    (0x1F6E9, 0x1F6E9),
    (0x1F6F0, 0x1F6F0),
    (0x1F6F3, 0x1F6F3),
];

/// True if emoji renders in color by default (Emoji_Presentation=Yes).
///
/// First char must be in the SMP emoji range (≥ U+1F000) and not one of the
/// SMP text-default exceptions, unless the entry carries VS16 (U+FE0F): text
/// shaping then takes the colour emoji font first (vendored parley
/// `select_font`), so ❤️ and ✈️ draw in colour. Bare BMP emoji (✈, ⚽, ❤, ☝)
/// default to monochrome text presentation and stay out. A ZWJ sequence (🏳️‍🌈)
/// is its own colour glyph in the emoji font, so the exception list does not apply.
pub fn is_color_default(emoji: &str) -> bool {
    if emoji.contains('\u{FE0F}') {
        return true;
    }
    let zwj_sequence = emoji.contains('\u{200D}');
    emoji.chars().next().is_some_and(|c| {
        let c = c as u32;
        c >= 0x1F000
            && (zwj_sequence
                || !SMP_TEXT_DEFAULT
                    .iter()
                    .any(|&(lo, hi)| (lo..=hi).contains(&c)))
    })
}

/// A table row with skin `tone` (1-5) applied, when the emoji takes tones and
/// the toned form draws in colour; otherwise the row as is. Tones come from
/// Unicode's own toned sequences, so 🧑‍🤝‍🧑 gets both people toned.
fn with_skin_tone(row: &TableRow, tone: i32) -> &'static str {
    match (row.3, tone) {
        (Some(tones), 1..=5) if is_color_default(tones[(tone - 1) as usize]) => tones[(tone - 1) as usize],
        _ => row.0,
    }
}

/// Where a freshly opened picker lands: Recent when the account has picked
/// anything, otherwise Smileys. Returns the tab and what it shows.
pub fn initial_listing(recent: &[String], tone: i32) -> (i32, Vec<PickerEmoji>) {
    let recents = recent_emojis(recent);
    if recents.is_empty() {
        (DEFAULT_CATEGORY, listing(DEFAULT_CATEGORY, tone, recent))
    } else {
        (RECENT_CATEGORY, recents)
    }
}

/// What a picker tab shows: the recents for [`RECENT_CATEGORY`], otherwise
/// the category with skin `tone` applied (People & Body only; 0 = none).
pub fn listing(category: i32, tone: i32, recent: &[String]) -> Vec<PickerEmoji> {
    if category == RECENT_CATEGORY {
        return recent_emojis(recent);
    }
    let tone = if category == PEOPLE_CATEGORY { tone } else { 0 };
    category_rows(category)
        .map(|row| PickerEmoji {
            emoji: with_skin_tone(row, tone).to_string(),
            name: row.1.to_string(),
            category,
        })
        .collect()
}

/// Recently used emoji, newest first.
fn recent_emojis(recent: &[String]) -> Vec<PickerEmoji> {
    recent
        .iter()
        // A text-default pick must not bring tofu back.
        .filter(|emoji| is_color_default(emoji))
        .map(|emoji| PickerEmoji {
            emoji: emoji.clone(),
            name: "recent".into(),
            category: RECENT_CATEGORY,
        })
        .collect()
}

type TableRow = (&'static str, &'static str, u8, Option<[&'static str; 5]>);

/// A category's colour-default rows, in Unicode order.
fn category_rows(category: i32) -> impl Iterator<Item = &'static TableRow> {
    super::emoji_table::EMOJI_TABLE
        .iter()
        .filter(move |row| i32::from(row.2) == category && is_color_default(row.0))
}

/// Categories: 0=Smileys, 1=People, 2=Animals, 3=Food, 4=Travel, 5=Activities, 6=Objects, 7=Symbols, 8=Flags
pub fn category_emojis(category: i32) -> Vec<PickerEmoji> {
    listing(category, 0, &[])
}

/// Search emoji by CLDR name across all categories, ignoring case (at most
/// 50): an exact name first, then names with a word starting with the query,
/// then any other match — "fire" lists 🔥 before 🚒 "fire engine".
pub fn search(query: &str) -> Vec<PickerEmoji> {
    let query = query.to_lowercase();
    let rank = |name: &str| {
        if name == query {
            0
        } else if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&query)) {
            1
        } else {
            2
        }
    };
    let mut results: Vec<(u8, &TableRow)> = super::emoji_table::EMOJI_TABLE
        .iter()
        .filter(|row| is_color_default(row.0))
        .filter_map(|row| {
            let name = row.1.to_lowercase();
            name.contains(&query).then(|| (rank(&name), row))
        })
        .collect();
    results.sort_by_key(|(rank, _)| *rank);
    results
        .into_iter()
        .take(50)
        .map(|(_, row)| PickerEmoji {
            emoji: row.0.to_string(),
            name: row.1.to_string(),
            category: i32::from(row.2),
        })
        .collect()
}

#[cfg(test)]
mod color_default_tests {
    use super::is_color_default;

    #[test]
    fn smp_text_default_emoji_are_excluded() {
        assert!(is_color_default("\u{1F600}")); // 😀
        assert!(is_color_default("\u{1F3C1}")); // 🏁
        assert!(!is_color_default("\u{1F6E1}")); // 🛡 bare
        assert!(!is_color_default("\u{1F441}")); // 👁
        assert!(!is_color_default("\u{2708}")); // ✈ bare (BMP)
        assert!(!is_color_default("\u{2764}")); // ❤ bare (BMP)
        // VS16 selects the colour glyph.
        assert!(is_color_default("\u{1F6E1}\u{FE0F}")); // 🛡️
        assert!(is_color_default("\u{2708}\u{FE0F}")); // ✈️
        assert!(is_color_default("\u{2764}\u{FE0F}")); // ❤️
        assert!(is_color_default("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}")); // 🏳️‍🌈
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    #[test]
    fn skin_tone_applies_to_people_only() {
        let recent: Vec<String> = Vec::new();
        let plain = listing(PEOPLE_CATEGORY, 0, &recent);
        let toned = listing(PEOPLE_CATEGORY, 3, &recent);
        assert_eq!(plain.len(), toned.len());
        assert_eq!(toned[0].emoji, "\u{1F44B}\u{1F3FD}"); // 👋🏽
        assert_eq!(listing(0, 3, &recent), listing(0, 0, &recent));
        // Out-of-range tones are ignored.
        assert_eq!(listing(PEOPLE_CATEGORY, 9, &recent), plain);
    }

    #[test]
    fn recents_and_search() {
        let recent = vec![
            "\u{1F6E1}".to_string(), // bare 🛡, text-default: never offered
            "😀".to_string(),
        ];
        let tab = listing(RECENT_CATEGORY, 0, &recent);
        assert_eq!(
            tab.iter().map(|e| e.emoji.as_str()).collect::<Vec<_>>(),
            ["😀"]
        );
        assert!(tab.iter().all(|e| e.category == RECENT_CATEGORY));

        assert_eq!(search("HEART"), search("heart"));
        let hits = search("heart");
        assert!(hits.iter().any(|e| e.name == "smiling face with heart-eyes"));
        assert!(hits.iter().any(|e| e.emoji == "\u{2764}\u{FE0F}"));
        assert!(hits.iter().all(|e| e.name.contains("heart")));
        assert!(search("").len() <= 50);
        assert!(listing(42, 0, &recent).is_empty());
    }

    #[test]
    fn fresh_picker_lands_on_recent_only_when_there_is_history() {
        let (category, shown) = initial_listing(&[], 0);
        assert_eq!(category, DEFAULT_CATEGORY);
        assert_eq!(shown, category_emojis(DEFAULT_CATEGORY));
        // Only tofu in history counts as no history.
        let (category, _) = initial_listing(&["\u{1F6E1}".to_string()], 0);
        assert_eq!(category, DEFAULT_CATEGORY);
        let (category, shown) = initial_listing(&["🎉".to_string()], 0);
        assert_eq!(category, RECENT_CATEGORY);
        assert_eq!(shown[0].emoji, "🎉");
    }
}
