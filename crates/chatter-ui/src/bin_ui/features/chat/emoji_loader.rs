//! Shared emoji loading logic for composer and reactions.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::ui::recent_emoji::RecentEmojiStore;

use crate::{EmojiData, MainWindow};

/// Category index reserved for "Recent" emoji (displayed as first tab).
const RECENT_CATEGORY: i32 = -1;

/// Fitzpatrick skin tone modifiers (indices 1-5). Index 0 = no modifier (default yellow).
const SKIN_TONE_MODIFIERS: [&str; 5] = [
    "\u{1F3FB}", // Light
    "\u{1F3FC}", // Medium-Light
    "\u{1F3FD}", // Medium
    "\u{1F3FE}", // Medium-Dark
    "\u{1F3FF}", // Dark
];

/// True if emoji renders in color by default (Emoji_Presentation=Yes).
///
/// Heuristic: first char must be in SMP emoji range (≥ U+1F000). BMP emoji
/// (e.g. ✈, ⚽, ❤, ☝) default to monochrome text presentation and render grey
/// without VS16 — we exclude them entirely for visual consistency.
fn is_color_default(emoji: &str) -> bool {
    emoji.chars().next().is_some_and(|c| (c as u32) >= 0x1F000)
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

/// Wire emoji loader callbacks.
pub fn wire_emoji_loader(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let store = Arc::new(RecentEmojiStore::load(ui_ctx.directories().data_dir()));
    let weak = window.as_weak();

    // Initial load — show smileys (category 0) by default
    if let Some(w) = weak.upgrade() {
        let emojis = get_emoji_list(0);
        w.set_composer_emoji_list(ModelRc::new(VecModel::from(emojis.clone())));
        w.set_reaction_picker_emojis(ModelRc::new(VecModel::from(emojis)));
    }

    window.on_composer_emoji_category_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                let tone = w.get_composer_emoji_skin_tone();
                let emojis = if category == RECENT_CATEGORY {
                    recent_to_emoji_data(&store)
                } else {
                    get_emoji_list_with_tone(category, tone)
                };
                w.set_composer_emoji_list(ModelRc::new(VecModel::from(emojis)));
            }
        }
    });

    window.on_composer_emoji_search_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                if query.is_empty() {
                    let cat = w.get_composer_emoji_category();
                    let emojis = if cat == RECENT_CATEGORY {
                        recent_to_emoji_data(&store)
                    } else {
                        get_emoji_list(cat)
                    };
                    w.set_composer_emoji_list(ModelRc::new(VecModel::from(emojis)));
                } else {
                    let emojis = search_emojis(&query);
                    w.set_composer_emoji_list(ModelRc::new(VecModel::from(emojis)));
                }
            }
        }
    });

    // Reaction picker emoji callbacks
    window.on_reaction_picker_category_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |category| {
            if let Some(w) = weak.upgrade() {
                let emojis = if category == RECENT_CATEGORY {
                    recent_to_emoji_data(&store)
                } else {
                    get_emoji_list(category)
                };
                w.set_reaction_picker_emojis(ModelRc::new(VecModel::from(emojis)));
            }
        }
    });

    window.on_reaction_picker_search_changed({
        let weak = weak.clone();
        let store = store.clone();
        move |query| {
            if let Some(w) = weak.upgrade() {
                if query.is_empty() {
                    let cat = w.get_reaction_picker_selected_category();
                    let emojis = if cat == RECENT_CATEGORY {
                        recent_to_emoji_data(&store)
                    } else {
                        get_emoji_list(cat)
                    };
                    w.set_reaction_picker_emojis(ModelRc::new(VecModel::from(emojis)));
                } else {
                    let emojis = search_emojis(&query);
                    w.set_reaction_picker_emojis(ModelRc::new(VecModel::from(emojis)));
                }
            }
        }
    });

    // Track emoji usage + insert into composer text + close picker
    window.on_composer_emoji_selected({
        let weak = weak.clone();
        let store = store.clone();
        move |emoji| {
            store.record(&emoji);
            let emoji_str = emoji.to_string();
            if let Some(w) = weak.upgrade() {
                let current_text = w.get_composer_text().to_string();
                let new_text = format!("{current_text}{emoji_str}");
                w.set_composer_text(new_text.into());
                // Deliberately left OPEN. Picking an emoji for the composer is
                // usually not the last thing you do — closing after each one
                // made "😀😀😀" four taps (§112u). Back, or the emoji button,
                // closes it. The REACTION picker still closes on pick.
            }
        }
    });

    // Skin tone changed — reload People category with selected tone
    window.on_composer_emoji_skin_tone_changed({
        let weak = weak.clone();
        move |tone| {
            if let Some(w) = weak.upgrade() {
                let category = w.get_composer_emoji_category();
                if category == 1 {
                    // People & Body — apply skin tone
                    let emojis = get_emoji_list_with_tone(category, tone);
                    w.set_composer_emoji_list(ModelRc::new(VecModel::from(emojis)));
                }
            }
        }
    });
}

/// Get the `RecentEmojiStore` as a standalone helper for other modules.
fn recent_to_emoji_data(store: &RecentEmojiStore) -> Vec<EmojiData> {
    store
        .get_recent()
        .into_iter()
        .map(|emoji| EmojiData {
            emoji: emoji.into(),
            name: "recent".into(),
            category: RECENT_CATEGORY,
        })
        .collect()
}

/// Get emoji list with skin tone applied (only affects People & Body, category 1).
fn get_emoji_list_with_tone(category: i32, tone: i32) -> Vec<EmojiData> {
    let base = get_emoji_list(category);
    if category != 1 || tone == 0 {
        return base;
    }
    base.into_iter()
        .map(|mut e| {
            e.emoji = apply_skin_tone(&e.emoji, tone).into();
            e
        })
        .collect()
}

/// Categories: 0=Smileys, 1=People, 2=Animals, 3=Food, 4=Travel, 5=Activities, 6=Objects, 7=Symbols, 8=Flags
pub fn get_emoji_list(category: i32) -> Vec<EmojiData> {
    let emojis: &[(&str, &str)] = match category {
        0 => &[ // Smileys & Emotion
            ("😀", "grinning"), ("😃", "smiley"), ("😄", "smile"), ("😁", "grin"),
            ("😅", "sweat_smile"), ("😂", "joy"), ("🤣", "rofl"), ("😊", "blush"),
            ("😇", "innocent"), ("🙂", "slight_smile"), ("😉", "wink"), ("😍", "heart_eyes"),
            ("🥰", "smiling_hearts"), ("😘", "kissing_heart"), ("😋", "yum"), ("😜", "stuck_out_tongue_winking"),
            ("🤪", "zany"), ("🤨", "raised_eyebrow"), ("🧐", "monocle"), ("🤓", "nerd"),
            ("😎", "sunglasses"), ("🥳", "partying"), ("😏", "smirk"), ("😒", "unamused"),
            ("😞", "disappointed"), ("😔", "pensive"), ("😟", "worried"), ("😕", "confused"),
            ("🙁", "slightly_frowning"), ("😣", "persevere"), ("😖", "confounded"), ("😫", "tired"),
            ("😩", "weary"), ("🥺", "pleading"), ("😢", "cry"), ("😭", "sob"),
            ("😤", "triumph"), ("😠", "angry"), ("😡", "rage"), ("🤬", "cursing"),
        ],
        1 => &[ // People & Body
            ("👋", "wave"), ("🤚", "raised_back"), ("🖐️", "hand_splayed"), ("✋", "hand"),
            ("🖖", "vulcan"), ("👌", "ok_hand"), ("🤌", "pinched"), ("🤏", "pinching"),
            ("✌️", "v"), ("🤞", "crossed_fingers"), ("🤟", "love_you"), ("🤘", "metal"),
            ("🤙", "call_me"), ("👈", "point_left"), ("👉", "point_right"), ("👆", "point_up"),
            ("👇", "point_down"), ("☝️", "point_up_2"), ("👍", "thumbsup"), ("👎", "thumbsdown"),
            ("✊", "fist"), ("👊", "punch"), ("🤛", "left_fist"), ("🤜", "right_fist"),
            ("👏", "clap"), ("🙌", "raised_hands"), ("👐", "open_hands"), ("🤲", "palms_up"),
            ("🤝", "handshake"), ("🙏", "pray"), ("💪", "muscle"), ("🦾", "mechanical_arm"),
        ],
        2 => &[ // Animals & Nature
            ("🐱", "cat"), ("🐶", "dog"), ("🐭", "mouse"), ("🐹", "hamster"),
            ("🐰", "rabbit"), ("🦊", "fox"), ("🐻", "bear"), ("🐼", "panda"),
            ("🐨", "koala"), ("🐯", "tiger"), ("🦁", "lion"), ("🐮", "cow"),
            ("🐷", "pig"), ("🐸", "frog"), ("🐵", "monkey"), ("🐔", "chicken"),
            ("🐧", "penguin"), ("🐦", "bird"), ("🐤", "baby_chick"), ("🦆", "duck"),
            ("🦅", "eagle"), ("🦉", "owl"), ("🦇", "bat"), ("🐺", "wolf"),
            ("🐗", "boar"), ("🐴", "horse"), ("🦄", "unicorn"), ("🐝", "bee"),
            ("🐛", "bug"), ("🦋", "butterfly"), ("🐌", "snail"), ("🐞", "ladybug"),
        ],
        3 => &[ // Food & Drink
            ("🍎", "apple"), ("🍐", "pear"), ("🍊", "orange"), ("🍋", "lemon"),
            ("🍌", "banana"), ("🍉", "watermelon"), ("🍇", "grapes"), ("🍓", "strawberry"),
            ("🫐", "blueberries"), ("🍈", "melon"), ("🍒", "cherries"), ("🍑", "peach"),
            ("🥭", "mango"), ("🍍", "pineapple"), ("🥥", "coconut"), ("🥝", "kiwi"),
            ("🍅", "tomato"), ("🥑", "avocado"), ("🥦", "broccoli"), ("🥬", "leafy_green"),
            ("🥒", "cucumber"), ("🌶️", "hot_pepper"), ("🫑", "bell_pepper"), ("🌽", "corn"),
            ("🥕", "carrot"), ("🧄", "garlic"), ("🧅", "onion"), ("🥔", "potato"),
            ("🍞", "bread"), ("🥐", "croissant"), ("🥖", "baguette"), ("🍕", "pizza"),
        ],
        4 => &[ // Travel & Places
            ("✈️", "airplane"), ("🚀", "rocket"), ("🚁", "helicopter"), ("🛸", "flying_saucer"),
            ("🚂", "steam_locomotive"), ("🚃", "railway_car"), ("🚄", "bullet_train"), ("🚅", "train"),
            ("🚆", "train2"), ("🚇", "metro"), ("🚈", "light_rail"), ("🚉", "station"),
            ("🚊", "tram"), ("🚝", "monorail"), ("🚞", "mountain_railway"), ("🚋", "train_car"),
            ("🚌", "bus"), ("🚍", "oncoming_bus"), ("🚎", "trolleybus"), ("🚐", "minibus"),
            ("🚑", "ambulance"), ("🚒", "fire_engine"), ("🚓", "police_car"), ("🚕", "taxi"),
            ("🚖", "oncoming_taxi"), ("🚗", "car"), ("🚘", "oncoming_car"), ("🚙", "suv"),
            ("🏠", "house"), ("🏡", "house_garden"), ("🏢", "office"), ("🏰", "castle"),
        ],
        5 => &[ // Activities
            ("⚽", "soccer"), ("🏀", "basketball"), ("🏈", "football"), ("⚾", "baseball"),
            ("🥎", "softball"), ("🎾", "tennis"), ("🏐", "volleyball"), ("🏉", "rugby"),
            ("🥏", "flying_disc"), ("🎱", "8ball"), ("🪀", "yo_yo"), ("🏓", "ping_pong"),
            ("🏸", "badminton"), ("🏒", "ice_hockey"), ("🏑", "field_hockey"), ("🥍", "lacrosse"),
            ("🏏", "cricket game"), ("🪃", "boomerang"), ("🥅", "goal_net"), ("⛳", "golf"),
            ("🪁", "kite"), ("🏹", "bow and arrow"), ("🎣", "fishing pole"), ("🤿", "diving mask"),
            ("🥊", "boxing glove"), ("🥋", "martial arts uniform"), ("🎿", "skis"), ("⛷️", "skier"),
            ("🏂", "snowboarder"), ("🪂", "parachute"), ("🏋️", "person lifting weights"), ("🤸", "person cartwheeling"),
        ],
        6 => &[ // Objects
            ("💡", "bulb"), ("🔦", "flashlight"), ("🏮", "lantern"), ("🪔", "diya_lamp"),
            ("📱", "iphone"), ("📲", "calling"), ("💻", "computer"), ("🖥️", "desktop"),
            ("🖨️", "printer"), ("⌨️", "keyboard"), ("🖱️", "mouse_computer"), ("💽", "minidisc"),
            ("💾", "floppy_disk"), ("💿", "cd"), ("📀", "dvd"), ("🎥", "movie_camera"),
            ("📷", "camera"), ("📸", "camera_flash"), ("📹", "video_camera"), ("📼", "vhs"),
            ("🔍", "mag"), ("🔎", "mag_right"), ("💰", "moneybag"), ("💳", "credit_card"),
            ("💎", "gem"), ("⚖️", "scales"), ("🔧", "wrench"), ("🔨", "hammer"),
            ("⚒️", "hammer_pick"), ("🛠️", "tools"), ("⛏️", "pick"), ("🔩", "nut_and_bolt"),
        ],
        7 => &[ // Symbols
            ("❤️", "heart"), ("🧡", "orange_heart"), ("💛", "yellow_heart"), ("💚", "green_heart"),
            ("💙", "blue_heart"), ("💜", "purple_heart"), ("🖤", "black_heart"), ("🤍", "white_heart"),
            ("🤎", "brown_heart"), ("💔", "broken_heart"), ("❣️", "heart_exclamation"), ("💕", "two_hearts"),
            ("💞", "revolving_hearts"), ("💓", "heartbeat"), ("💗", "heartpulse"), ("💖", "sparkling_heart"),
            ("💘", "cupid"), ("💝", "gift_heart"), ("✅", "white_check_mark"), ("❌", "x"),
            ("⭕", "o"), ("❗", "exclamation"), ("❓", "question"), ("⚠️", "warning"),
            ("🔴", "red_circle"), ("🟠", "orange_circle"), ("🟡", "yellow_circle"), ("🟢", "green_circle"),
            ("🔵", "blue_circle"), ("🟣", "purple_circle"), ("⚫", "black_circle"), ("⚪", "white_circle"),
        ],
        8 => &[ // Flags
            ("🏳️", "white_flag"), ("🏴", "black_flag"), ("🏁", "checkered_flag"), ("🚩", "triangular_flag"),
            ("🏳️‍🌈", "rainbow_flag"), ("🏳️‍⚧️", "transgender_flag"), ("🏴‍☠️", "pirate_flag"),
            ("🇺🇸", "us"), ("🇬🇧", "gb"), ("🇨🇦", "ca"), ("🇦🇺", "au"),
            ("🇩🇪", "de"), ("🇫🇷", "fr"), ("🇮🇹", "it"), ("🇪🇸", "es"),
            ("🇯🇵", "jp"), ("🇰🇷", "kr"), ("🇨🇳", "cn"), ("🇮🇳", "in"),
            ("🇧🇷", "br"), ("🇲🇽", "mx"), ("🇷🇺", "ru"), ("🇿🇦", "za"),
            ("🇳🇱", "nl"), ("🇧🇪", "be"), ("🇨🇭", "ch"), ("🇦🇹", "at"),
            ("🇸🇪", "se"), ("🇳🇴", "no"), ("🇩🇰", "dk"), ("🇫🇮", "fi"),
        ],
        _ => &[],
    };

    emojis
        .iter()
        .filter(|(emoji, _)| is_color_default(emoji))
        .map(|(emoji, name)| EmojiData {
            emoji: emoji.to_string().into(),
            name: name.to_string().into(),
            category,
        })
        .collect()
}

/// Search emojis by name across all categories.
pub fn search_emojis(query: &str) -> Vec<EmojiData> {
    let mut results = Vec::new();
    for category in 0..=8 {
        let emojis = get_emoji_list(category);
        for emoji in emojis {
            if emoji.name.to_lowercase().contains(query) {
                results.push(emoji);
            }
        }
    }
    results.truncate(50); // Limit results
    results
}
