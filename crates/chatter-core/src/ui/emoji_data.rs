//! The emoji picker's data: the table, search, skin tones and recents, shared
//! by the composer picker and the reaction picker. The UI renders what these
//! return.
//!
//! Only colour-default emoji are offered (see [`is_color_default`]): a
//! text-default one renders grey, or as tofu on older Android fonts.

use super::recent_emoji::RecentEmojiStore;

/// Category index reserved for "Recent" emoji (displayed as first tab).
pub const RECENT_CATEGORY: i32 = -1;
/// People & Body: the one category skin tones apply to.
pub const PEOPLE_CATEGORY: i32 = 1;

/// One picker cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerEmoji {
    pub emoji: String,
    pub name: String,
    pub category: i32,
}

/// Fitzpatrick skin tone modifiers (indices 1-5). Index 0 = no modifier (default yellow).
const SKIN_TONE_MODIFIERS: [&str; 5] = [
    "\u{1F3FB}", // Light
    "\u{1F3FC}", // Medium-Light
    "\u{1F3FD}", // Medium
    "\u{1F3FE}", // Medium-Dark
    "\u{1F3FF}", // Dark
];

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
/// SMP text-default exceptions. BMP emoji (e.g. ✈, ⚽, ❤, ☝) default to
/// monochrome text presentation and render grey without VS16 — we exclude
/// them entirely for visual consistency. A ZWJ sequence (🏳️‍🌈) is its own
/// colour glyph in the emoji font, so the exception list does not apply.
pub fn is_color_default(emoji: &str) -> bool {
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

/// Apply a skin tone modifier to an emoji if it supports it (People & Body category).
fn apply_skin_tone(emoji: &str, tone: i32) -> String {
    if tone == 0 || tone < 0 || tone > 5 {
        return emoji.to_string();
    }
    let modifier = SKIN_TONE_MODIFIERS[(tone - 1) as usize];
    // Insert modifier after the base emoji character (before any variant selectors)
    let mut chars = emoji.chars();
    if let Some(first) = chars.next() {
        let rest: String = chars.collect();
        format!("{first}{modifier}{rest}")
    } else {
        emoji.to_string()
    }
}

/// What a picker tab shows: the recents for [`RECENT_CATEGORY`], otherwise
/// the category with skin `tone` applied (People & Body only; 0 = none).
pub fn listing(category: i32, tone: i32, recent: &RecentEmojiStore) -> Vec<PickerEmoji> {
    if category == RECENT_CATEGORY {
        return recent_emojis(recent);
    }
    let base = category_emojis(category);
    if category != PEOPLE_CATEGORY || tone == 0 {
        return base;
    }
    base.into_iter()
        .map(|mut e| {
            e.emoji = apply_skin_tone(&e.emoji, tone);
            e
        })
        .collect()
}

/// Recently used emoji, newest first.
fn recent_emojis(store: &RecentEmojiStore) -> Vec<PickerEmoji> {
    store
        .get_recent()
        .into_iter()
        // Picks saved before the text-default filter must not bring tofu back.
        .filter(|emoji| is_color_default(emoji))
        .map(|emoji| PickerEmoji {
            emoji,
            name: "recent".into(),
            category: RECENT_CATEGORY,
        })
        .collect()
}

/// Categories: 0=Smileys, 1=People, 2=Animals, 3=Food, 4=Travel, 5=Activities, 6=Objects, 7=Symbols, 8=Flags
pub fn category_emojis(category: i32) -> Vec<PickerEmoji> {
    let emojis: &[(&str, &str)] = match category {
        0 => &[
            // Smileys & Emotion
            ("😀", "grinning"),
            ("😃", "smiley"),
            ("😄", "smile"),
            ("😁", "grin"),
            ("😅", "sweat_smile"),
            ("😂", "joy"),
            ("🤣", "rofl"),
            ("😊", "blush"),
            ("😇", "innocent"),
            ("🙂", "slight_smile"),
            ("😉", "wink"),
            ("😍", "heart_eyes"),
            ("🥰", "smiling_hearts"),
            ("😘", "kissing_heart"),
            ("😋", "yum"),
            ("😜", "stuck_out_tongue_winking"),
            ("🤪", "zany"),
            ("🤨", "raised_eyebrow"),
            ("🧐", "monocle"),
            ("🤓", "nerd"),
            ("😎", "sunglasses"),
            ("🥳", "partying"),
            ("😏", "smirk"),
            ("😒", "unamused"),
            ("😞", "disappointed"),
            ("😔", "pensive"),
            ("😟", "worried"),
            ("😕", "confused"),
            ("🙁", "slightly_frowning"),
            ("😣", "persevere"),
            ("😖", "confounded"),
            ("😫", "tired"),
            ("😩", "weary"),
            ("🥺", "pleading"),
            ("😢", "cry"),
            ("😭", "sob"),
            ("😤", "triumph"),
            ("😠", "angry"),
            ("😡", "rage"),
            ("🤬", "cursing"),
        ],
        1 => &[
            // People & Body
            ("👋", "wave"),
            ("🤚", "raised_back"),
            ("🖐️", "hand_splayed"),
            ("✋", "hand"),
            ("🖖", "vulcan"),
            ("👌", "ok_hand"),
            ("🤌", "pinched"),
            ("🤏", "pinching"),
            ("✌️", "v"),
            ("🤞", "crossed_fingers"),
            ("🤟", "love_you"),
            ("🤘", "metal"),
            ("🤙", "call_me"),
            ("👈", "point_left"),
            ("👉", "point_right"),
            ("👆", "point_up"),
            ("👇", "point_down"),
            ("☝️", "point_up_2"),
            ("👍", "thumbsup"),
            ("👎", "thumbsdown"),
            ("✊", "fist"),
            ("👊", "punch"),
            ("🤛", "left_fist"),
            ("🤜", "right_fist"),
            ("👏", "clap"),
            ("🙌", "raised_hands"),
            ("👐", "open_hands"),
            ("🤲", "palms_up"),
            ("🤝", "handshake"),
            ("🙏", "pray"),
            ("💪", "muscle"),
            ("🦾", "mechanical_arm"),
        ],
        2 => &[
            // Animals & Nature
            ("🐱", "cat"),
            ("🐶", "dog"),
            ("🐭", "mouse"),
            ("🐹", "hamster"),
            ("🐰", "rabbit"),
            ("🦊", "fox"),
            ("🐻", "bear"),
            ("🐼", "panda"),
            ("🐨", "koala"),
            ("🐯", "tiger"),
            ("🦁", "lion"),
            ("🐮", "cow"),
            ("🐷", "pig"),
            ("🐸", "frog"),
            ("🐵", "monkey"),
            ("🐔", "chicken"),
            ("🐧", "penguin"),
            ("🐦", "bird"),
            ("🐤", "baby_chick"),
            ("🦆", "duck"),
            ("🦅", "eagle"),
            ("🦉", "owl"),
            ("🦇", "bat"),
            ("🐺", "wolf"),
            ("🐗", "boar"),
            ("🐴", "horse"),
            ("🦄", "unicorn"),
            ("🐝", "bee"),
            ("🐛", "bug"),
            ("🦋", "butterfly"),
            ("🐌", "snail"),
            ("🐞", "ladybug"),
        ],
        3 => &[
            // Food & Drink
            ("🍎", "apple"),
            ("🍐", "pear"),
            ("🍊", "orange"),
            ("🍋", "lemon"),
            ("🍌", "banana"),
            ("🍉", "watermelon"),
            ("🍇", "grapes"),
            ("🍓", "strawberry"),
            ("🫐", "blueberries"),
            ("🍈", "melon"),
            ("🍒", "cherries"),
            ("🍑", "peach"),
            ("🥭", "mango"),
            ("🍍", "pineapple"),
            ("🥥", "coconut"),
            ("🥝", "kiwi"),
            ("🍅", "tomato"),
            ("🥑", "avocado"),
            ("🥦", "broccoli"),
            ("🥬", "leafy_green"),
            ("🥒", "cucumber"),
            ("🌶️", "hot_pepper"),
            ("🫑", "bell_pepper"),
            ("🌽", "corn"),
            ("🥕", "carrot"),
            ("🧄", "garlic"),
            ("🧅", "onion"),
            ("🥔", "potato"),
            ("🍞", "bread"),
            ("🥐", "croissant"),
            ("🥖", "baguette"),
            ("🍕", "pizza"),
        ],
        4 => &[
            // Travel & Places
            ("✈️", "airplane"),
            ("🚀", "rocket"),
            ("🚁", "helicopter"),
            ("🛸", "flying_saucer"),
            ("🚂", "steam_locomotive"),
            ("🚃", "railway_car"),
            ("🚄", "bullet_train"),
            ("🚅", "train"),
            ("🚆", "train2"),
            ("🚇", "metro"),
            ("🚈", "light_rail"),
            ("🚉", "station"),
            ("🚊", "tram"),
            ("🚝", "monorail"),
            ("🚞", "mountain_railway"),
            ("🚋", "train_car"),
            ("🚌", "bus"),
            ("🚍", "oncoming_bus"),
            ("🚎", "trolleybus"),
            ("🚐", "minibus"),
            ("🚑", "ambulance"),
            ("🚒", "fire_engine"),
            ("🚓", "police_car"),
            ("🚕", "taxi"),
            ("🚖", "oncoming_taxi"),
            ("🚗", "car"),
            ("🚘", "oncoming_car"),
            ("🚙", "suv"),
            ("🏠", "house"),
            ("🏡", "house_garden"),
            ("🏢", "office"),
            ("🏰", "castle"),
        ],
        5 => &[
            // Activities
            ("⚽", "soccer"),
            ("🏀", "basketball"),
            ("🏈", "football"),
            ("⚾", "baseball"),
            ("🥎", "softball"),
            ("🎾", "tennis"),
            ("🏐", "volleyball"),
            ("🏉", "rugby"),
            ("🥏", "flying_disc"),
            ("🎱", "8ball"),
            ("🪀", "yo_yo"),
            ("🏓", "ping_pong"),
            ("🏸", "badminton"),
            ("🏒", "ice_hockey"),
            ("🏑", "field_hockey"),
            ("🥍", "lacrosse"),
            ("🏏", "cricket game"),
            ("🪃", "boomerang"),
            ("🥅", "goal_net"),
            ("⛳", "golf"),
            ("🪁", "kite"),
            ("🏹", "bow and arrow"),
            ("🎣", "fishing pole"),
            ("🤿", "diving mask"),
            ("🥊", "boxing glove"),
            ("🥋", "martial arts uniform"),
            ("🎿", "skis"),
            ("⛷️", "skier"),
            ("🏂", "snowboarder"),
            ("🪂", "parachute"),
            ("🏋️", "person lifting weights"),
            ("🤸", "person cartwheeling"),
        ],
        6 => &[
            // Objects
            ("💡", "bulb"),
            ("🔦", "flashlight"),
            ("🏮", "lantern"),
            ("🪔", "diya_lamp"),
            ("📱", "iphone"),
            ("📲", "calling"),
            ("💻", "computer"),
            ("🖥️", "desktop"),
            ("🖨️", "printer"),
            ("⌨️", "keyboard"),
            ("🖱️", "mouse_computer"),
            ("💽", "minidisc"),
            ("💾", "floppy_disk"),
            ("💿", "cd"),
            ("📀", "dvd"),
            ("🎥", "movie_camera"),
            ("📷", "camera"),
            ("📸", "camera_flash"),
            ("📹", "video_camera"),
            ("📼", "vhs"),
            ("🔍", "mag"),
            ("🔎", "mag_right"),
            ("💰", "moneybag"),
            ("💳", "credit_card"),
            ("💎", "gem"),
            ("⚖️", "scales"),
            ("🔧", "wrench"),
            ("🔨", "hammer"),
            ("⚒️", "hammer_pick"),
            ("🛠️", "tools"),
            ("⛏️", "pick"),
            ("🔩", "nut_and_bolt"),
        ],
        7 => &[
            // Symbols
            ("❤️", "heart"),
            ("🧡", "orange_heart"),
            ("💛", "yellow_heart"),
            ("💚", "green_heart"),
            ("💙", "blue_heart"),
            ("💜", "purple_heart"),
            ("🖤", "black_heart"),
            ("🤍", "white_heart"),
            ("🤎", "brown_heart"),
            ("💔", "broken_heart"),
            ("❣️", "heart_exclamation"),
            ("💕", "two_hearts"),
            ("💞", "revolving_hearts"),
            ("💓", "heartbeat"),
            ("💗", "heartpulse"),
            ("💖", "sparkling_heart"),
            ("💘", "cupid"),
            ("💝", "gift_heart"),
            ("✅", "white_check_mark"),
            ("❌", "x"),
            ("⭕", "o"),
            ("❗", "exclamation"),
            ("❓", "question"),
            ("⚠️", "warning"),
            ("🔴", "red_circle"),
            ("🟠", "orange_circle"),
            ("🟡", "yellow_circle"),
            ("🟢", "green_circle"),
            ("🔵", "blue_circle"),
            ("🟣", "purple_circle"),
            ("⚫", "black_circle"),
            ("⚪", "white_circle"),
        ],
        8 => &[
            // Flags
            ("🏳️", "white_flag"),
            ("🏴", "black_flag"),
            ("🏁", "checkered_flag"),
            ("🚩", "triangular_flag"),
            ("🏳️‍🌈", "rainbow_flag"),
            ("🏳️‍⚧️", "transgender_flag"),
            ("🏴‍☠️", "pirate_flag"),
            ("🇺🇸", "us"),
            ("🇬🇧", "gb"),
            ("🇨🇦", "ca"),
            ("🇦🇺", "au"),
            ("🇩🇪", "de"),
            ("🇫🇷", "fr"),
            ("🇮🇹", "it"),
            ("🇪🇸", "es"),
            ("🇯🇵", "jp"),
            ("🇰🇷", "kr"),
            ("🇨🇳", "cn"),
            ("🇮🇳", "in"),
            ("🇧🇷", "br"),
            ("🇲🇽", "mx"),
            ("🇷🇺", "ru"),
            ("🇿🇦", "za"),
            ("🇳🇱", "nl"),
            ("🇧🇪", "be"),
            ("🇨🇭", "ch"),
            ("🇦🇹", "at"),
            ("🇸🇪", "se"),
            ("🇳🇴", "no"),
            ("🇩🇰", "dk"),
            ("🇫🇮", "fi"),
        ],
        _ => &[],
    };

    emojis
        .iter()
        .filter(|(emoji, _)| is_color_default(emoji))
        .map(|(emoji, name)| PickerEmoji {
            emoji: (*emoji).to_string(),
            name: (*name).to_string(),
            category,
        })
        .collect()
}

/// Search emoji by name across all categories, ignoring case (at most 50).
pub fn search(query: &str) -> Vec<PickerEmoji> {
    let query = query.to_lowercase();
    let mut results: Vec<PickerEmoji> = (0..=8)
        .flat_map(category_emojis)
        .filter(|e| e.name.to_lowercase().contains(&query))
        .collect();
    results.truncate(50);
    results
}

#[cfg(test)]
mod color_default_tests {
    use super::is_color_default;

    #[test]
    fn smp_text_default_emoji_are_excluded() {
        assert!(is_color_default("\u{1F600}")); // 😀
        assert!(is_color_default("\u{1F3C1}")); // 🏁
        assert!(!is_color_default("\u{1F6E1}\u{FE0F}")); // 🛡️
        assert!(!is_color_default("\u{1F3F3}\u{FE0F}")); // 🏳️
        assert!(!is_color_default("\u{1F441}")); // 👁
        assert!(!is_color_default("\u{2708}\u{FE0F}")); // ✈️ (BMP)
        assert!(is_color_default("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}")); // 🏳️‍🌈
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    #[test]
    fn skin_tone_applies_to_people_only() {
        let dir = tempfile::tempdir().unwrap();
        let recent = RecentEmojiStore::load(dir.path());
        let plain = listing(PEOPLE_CATEGORY, 0, &recent);
        let toned = listing(PEOPLE_CATEGORY, 3, &recent);
        assert_eq!(plain.len(), toned.len());
        assert_eq!(toned[0].emoji, format!("👋{}", SKIN_TONE_MODIFIERS[2]));
        assert_eq!(listing(0, 3, &recent), listing(0, 0, &recent));
        // Out-of-range tones are ignored.
        assert_eq!(listing(PEOPLE_CATEGORY, 9, &recent), plain);
    }

    #[test]
    fn recents_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let recent = RecentEmojiStore::load(dir.path());
        recent.record("😀");
        recent.record("\u{1F6E1}\u{FE0F}"); // 🛡️, text-default: never offered
        let tab = listing(RECENT_CATEGORY, 0, &recent);
        assert_eq!(
            tab.iter().map(|e| e.emoji.as_str()).collect::<Vec<_>>(),
            ["😀"]
        );
        assert!(tab.iter().all(|e| e.category == RECENT_CATEGORY));

        assert_eq!(search("HEART"), search("heart"));
        let hits = search("heart");
        assert!(hits.iter().any(|e| e.name == "heart_eyes"));
        assert!(hits.iter().all(|e| e.name.contains("heart")));
        assert!(search("").len() <= 50);
        assert!(listing(42, 0, &recent).is_empty());
    }
}
