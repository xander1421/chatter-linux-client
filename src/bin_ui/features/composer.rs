//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions).

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::AttachmentId;
use chatter_linux::media::gif::GifClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::network::grpc::{ChannelClient, UserClient};
use chatter_linux::state::ChatState;
use chatter_linux::storage::AttachmentRepository;

use crate::bin_ui::converters::pending_attachment_to_slint;
use crate::{EmojiData, GifData, MainWindow, MentionSuggestion, PendingAttachmentData};

/// Wire composer callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_attachment_clicked(
        window,
        ctx.grpc_pool.clone(),
        ctx.chat_state.clone(),
        ctx.attachment_repo.clone(),
    );
    wire_remove_attachment(window, ctx.chat_state.clone());
    wire_emoji_clicked(window);
    wire_emoji_selected(window);
    wire_emoji_category_changed(window);
    wire_emoji_search_changed(window);
    wire_gif_clicked(window, ctx.gif_client.clone());
    wire_gif_selected(window);
    wire_gif_search(window, ctx.gif_client.clone());
    wire_mention_triggered(window, ctx.grpc_pool.clone(), ctx.chat_state.clone());
    wire_mention_selected(window);
    wire_process_text(window);
}

/// Maximum characters in a word before inserting a break point.
const MAX_WORD_LENGTH: usize = 50;

/// Insert zero-width spaces into long sequences without whitespace.
/// This allows Slint's word-wrap to break long strings.
fn insert_word_breaks(text: &str) -> String {
    const ZWSP: char = '\u{200B}';

    let mut result = String::with_capacity(text.len() + text.len() / MAX_WORD_LENGTH);
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() || ch == ZWSP {
            word_len = 0;
            result.push(ch);
        } else {
            word_len += 1;
            result.push(ch);
            if word_len >= MAX_WORD_LENGTH {
                result.push(ZWSP);
                word_len = 0;
            }
        }
    }

    result
}

/// Check if text needs word break processing.
/// Returns true if there's a sequence of non-whitespace chars longer than MAX_WORD_LENGTH
/// without a zero-width space.
fn needs_word_breaks(text: &str) -> bool {
    const ZWSP: char = '\u{200B}';
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() || ch == ZWSP {
            word_len = 0;
        } else {
            word_len += 1;
            if word_len > MAX_WORD_LENGTH {
                return true;
            }
        }
    }
    false
}

fn wire_process_text(window: &MainWindow) {
    window.on_process_text(|text: SharedString| -> SharedString {
        let text_str = text.as_str();
        if needs_word_breaks(text_str) {
            insert_word_breaks(text_str).into()
        } else {
            text
        }
    });
}

fn wire_attachment_clicked(
    window: &MainWindow,
    grpc_pool: Arc<GrpcClientPool>,
    chat_state: ChatState,
    attachment_repo: Arc<AttachmentRepository>,
) {
    let weak_window = window.as_weak();

    window.on_attachment_clicked(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let chat_state = chat_state.clone();
        let attachment_repo = attachment_repo.clone();
        tracing::debug!("Attachment clicked");

        // Use tokio::spawn for the main async work to keep the runtime alive
        tokio::spawn(async move {
            use chatter_linux::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};
            use chatter_linux::services::AttachmentService;
            use chatter_linux::state::chat_state::PendingAttachment;

            let config = FilePickerConfig::new()
                .with_title("Select Attachment")
                .with_filter(FileFilter::all())  // Default to all files
                .with_filter(FileFilter::images())
                .with_filter(FileFilter::videos())
                .with_filter(FileFilter::audio())
                .with_filter(FileFilter::documents())
                .with_multiple(true);

            let selected_files = FilePicker::pick_files(config).await;

            if selected_files.is_empty() {
                tracing::debug!("File picker cancelled or no files selected");
                return;
            }

            tracing::info!("Selected {} file(s) for attachment", selected_files.len());

            // Get active channel ID for upload permission
            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot upload: no active channel selected");
                    return;
                }
            };

            // Create attachment service
            let attachment_service = AttachmentService::new(attachment_repo);

            // Upload each file
            let mut uploaded_count = 0;
            for file_path in &selected_files {
                tracing::debug!("Uploading file: {:?}", file_path);

                match attachment_service.upload_file(&grpc_pool, file_path, &channel_id).await {
                    Ok(result) => {
                        uploaded_count += 1;
                        tracing::info!("Successfully uploaded: {:?}", file_path);

                        // Add to pending attachments for composer UI
                        let pending = PendingAttachment::new(
                            result.attachment_id,
                            result.filename,
                            file_path.to_path_buf(),
                            result.file_size,
                            result.mime_type,
                        );
                        chat_state.add_pending_attachment(pending);
                    }
                    Err(e) => {
                        tracing::error!("Failed to upload {:?}: {}", file_path, e);
                    }
                }
            }

            // Update UI with attachment model
            let attachments = chat_state.get_pending_attachments();
            tracing::debug!(
                "Updating UI with {} pending attachments",
                attachments.len()
            );
            for att in &attachments {
                tracing::debug!(
                    "  - {} ({} bytes, {})",
                    att.filename,
                    att.size,
                    att.mime_type
                );
            }

            let weak_final = weak.clone();
            match slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_final.upgrade() {
                    let slint_attachments: Vec<PendingAttachmentData> = attachments
                        .iter()
                        .map(pending_attachment_to_slint)
                        .collect();

                    tracing::debug!(
                        "Setting {} attachments to UI model",
                        slint_attachments.len()
                    );

                    let model = ModelRc::new(VecModel::from(slint_attachments));
                    w.set_pending_attachments(model);

                    // Update placeholder text
                    let pending_count = attachments.len();
                    let placeholder = if pending_count == 0 {
                        "Type a message...".to_string()
                    } else if pending_count == 1 {
                        "1 file attached - type a message...".to_string()
                    } else {
                        format!("{} files attached - type a message...", pending_count)
                    };
                    w.set_composer_placeholder(placeholder.into());
                    tracing::debug!("UI updated with attachments");
                } else {
                    tracing::warn!("Failed to upgrade window weak reference");
                }
            }) {
                Ok(_) => tracing::debug!("invoke_from_event_loop queued successfully"),
                Err(e) => tracing::error!("invoke_from_event_loop failed: {:?}", e),
            }

            if uploaded_count < selected_files.len() {
                tracing::warn!(
                    "Only uploaded {} of {} selected files",
                    uploaded_count,
                    selected_files.len()
                );
            }
        });
    });
}

/// Wire up the remove-attachment callback.
fn wire_remove_attachment(window: &MainWindow, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_remove_attachment(move |attachment_id: SharedString| {
        let id_str = attachment_id.to_string();
        tracing::debug!("Remove attachment clicked: {}", id_str);

        // Parse the attachment ID
        let parsed_id: Result<AttachmentId, _> = id_str.parse();
        match parsed_id {
            Ok(id) => {
                chat_state.remove_pending_attachment(id);
                tracing::info!("Removed pending attachment: {}", id_str);

                // Refresh the UI
                let attachments = chat_state.get_pending_attachments();
                let weak = weak_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let slint_attachments: Vec<PendingAttachmentData> = attachments
                            .iter()
                            .map(pending_attachment_to_slint)
                            .collect();

                        let model = ModelRc::new(VecModel::from(slint_attachments));
                        w.set_pending_attachments(model);

                        // Update placeholder text
                        let pending_count = attachments.len();
                        let placeholder = if pending_count == 0 {
                            "Type a message...".to_string()
                        } else if pending_count == 1 {
                            "1 file attached - type a message...".to_string()
                        } else {
                            format!("{} files attached - type a message...", pending_count)
                        };
                        w.set_composer_placeholder(placeholder.into());
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to parse attachment ID '{}': {}", id_str, e);
            }
        }
    });
}

fn wire_emoji_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_emoji_clicked(move || {
        let weak = weak_window.clone();
        tracing::debug!("Emoji picker clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Toggle the composer emoji picker
                let is_visible = w.get_composer_emoji_picker_visible();
                w.set_composer_emoji_picker_visible(!is_visible);

                // Load initial emoji list if opening
                if !is_visible {
                    let emojis = get_emoji_list(0); // Category 0 = Smileys
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                    w.set_composer_emoji_category(0);
                    w.set_composer_emoji_search("".into());
                }
            }
        });
    });
}

fn wire_emoji_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_selected(move |emoji: SharedString| {
        let weak = weak_window.clone();
        let emoji_str = emoji.to_string();
        tracing::debug!("Emoji selected: {}", emoji_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Insert emoji into composer text
                let current_text = w.get_composer_text().to_string();
                let new_text = format!("{}{}", current_text, emoji_str);
                w.set_composer_text(new_text.into());
                // Close the picker
                w.set_composer_emoji_picker_visible(false);
            }
        });
    });
}

fn wire_emoji_category_changed(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_category_changed(move |category: i32| {
        let weak = weak_window.clone();
        tracing::debug!("Emoji category changed: {}", category);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let emojis = get_emoji_list(category);
                w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
            }
        });
    });
}

fn wire_emoji_search_changed(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_composer_emoji_search_changed(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string().to_lowercase();
        tracing::debug!("Emoji search: {}", query_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if query_str.is_empty() {
                    // Return to category view
                    let category = w.get_composer_emoji_category();
                    let emojis = get_emoji_list(category);
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                } else {
                    // Search all emojis
                    let emojis = search_emojis(&query_str);
                    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(emojis)));
                }
            }
        });
    });
}

/// Get emoji list for a category.
/// Categories: 0=Smileys, 1=People, 2=Animals, 3=Food, 4=Travel, 5=Activities, 6=Objects, 7=Symbols, 8=Flags
fn get_emoji_list(category: i32) -> Vec<EmojiData> {
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
            ("🏏", "cricket"), ("🪃", "boomerang"), ("🥅", "goal_net"), ("⛳", "golf"),
            ("🪁", "kite"), ("🏹", "bow_and_arrow"), ("🎣", "fishing_pole"), ("🤿", "diving_mask"),
            ("🥊", "boxing_glove"), ("🥋", "martial_arts"), ("🎿", "skis"), ("⛷️", "skier"),
            ("🏂", "snowboarder"), ("🪂", "parachute"), ("🏋️", "weight_lifting"), ("🤸", "cartwheel"),
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
        .map(|(emoji, name)| EmojiData {
            emoji: emoji.to_string().into(),
            name: name.to_string().into(),
            category,
        })
        .collect()
}

/// Search emojis by name across all categories.
fn search_emojis(query: &str) -> Vec<EmojiData> {
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

fn wire_gif_clicked(window: &MainWindow, gif_client: Option<Arc<GifClient>>) {
    let weak_window = window.as_weak();

    window.on_gif_clicked(move || {
        let weak = weak_window.clone();
        let gif_client = gif_client.clone();
        tracing::debug!("GIF picker clicked");

        // First, toggle visibility synchronously
        let should_load = {
            let weak_sync = weak.clone();
            if let Some(w) = weak_sync.upgrade() {
                let is_visible = w.get_gif_picker_visible();
                w.set_gif_picker_visible(!is_visible);
                w.set_composer_emoji_picker_visible(false);
                w.set_mention_autocomplete_visible(false);

                if !is_visible {
                    w.set_gif_picker_loading(true);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        // Load trending GIFs asynchronously if opening
        if should_load {
            tokio::spawn(async move {
                let raw_gifs = fetch_trending_gifs_raw(gif_client.as_deref()).await;

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let gifs: Vec<GifData> = raw_gifs
                            .into_iter()
                            .map(|(id, _preview_url, full_url, width, height)| GifData {
                                id: id.into(),
                                preview: slint::Image::default(), // TODO: Load from preview_url
                                full_url: full_url.into(),
                                width: width as i32,
                                height: height as i32,
                            })
                            .collect();

                        w.set_gif_picker_results(slint::ModelRc::new(slint::VecModel::from(gifs)));
                        w.set_gif_picker_loading(false);
                    }
                });
            });
        }
    });
}

fn wire_gif_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_gif_selected(move |gif_url: SharedString| {
        let weak = weak_window.clone();
        let url = gif_url.to_string();
        tracing::debug!("GIF selected: {}", url);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Insert the GIF as a message (as a URL or embed)
                let current_text = w.get_composer_text().to_string();
                let new_text = if current_text.is_empty() {
                    url
                } else {
                    format!("{} {}", current_text, url)
                };
                w.set_composer_text(new_text.into());
                w.set_gif_picker_visible(false);
            }
        });
    });
}

fn wire_gif_search(window: &MainWindow, gif_client: Option<Arc<GifClient>>) {
    let weak_window = window.as_weak();

    window.on_gif_search(move |query: SharedString| {
        let weak = weak_window.clone();
        let query_str = query.to_string();
        let gif_client = gif_client.clone();
        tracing::debug!("GIF search: {}", query_str);

        // Set loading state synchronously
        if let Some(w) = weak.upgrade() {
            w.set_gif_picker_loading(true);
        }

        // Fetch GIFs asynchronously
        tokio::spawn(async move {
            let raw_gifs = if query_str.is_empty() {
                fetch_trending_gifs_raw(gif_client.as_deref()).await
            } else {
                search_gifs_raw(&query_str, gif_client.as_deref()).await
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let gifs: Vec<GifData> = raw_gifs
                        .into_iter()
                        .map(|(id, _preview_url, full_url, width, height)| GifData {
                            id: id.into(),
                            preview: slint::Image::default(), // TODO: Load from preview_url
                            full_url: full_url.into(),
                            width: width as i32,
                            height: height as i32,
                        })
                        .collect();

                    w.set_gif_picker_results(slint::ModelRc::new(slint::VecModel::from(gifs)));
                    w.set_gif_picker_loading(false);
                }
            });
        });
    });
}

/// Raw GIF data (Send-safe, no Slint types).
/// (id, preview_url, full_url, width, height)
type RawGifData = (String, String, String, u32, u32);

/// Fetch trending GIFs from Tenor API (async).
async fn fetch_trending_gifs_raw(gif_client: Option<&GifClient>) -> Vec<RawGifData> {
    match gif_client {
        Some(client) => {
            match client.trending(20).await {
                Ok(results) => {
                    tracing::debug!("Fetched {} trending GIFs from Tenor", results.len());
                    results
                        .into_iter()
                        .map(|r| (r.id, r.preview_url, r.url, r.width, r.height))
                        .collect()
                }
                Err(e) => {
                    tracing::error!("Failed to fetch trending GIFs: {}", e);
                    Vec::new()
                }
            }
        }
        None => {
            tracing::debug!("GIF client not available - no trending GIFs");
            Vec::new()
        }
    }
}

/// Search GIFs by query using Tenor API (async).
async fn search_gifs_raw(query: &str, gif_client: Option<&GifClient>) -> Vec<RawGifData> {
    match gif_client {
        Some(client) => {
            match client.search(query, 20).await {
                Ok(results) => {
                    tracing::debug!("Found {} GIFs for query '{}'", results.len(), query);
                    results
                        .into_iter()
                        .map(|r| (r.id, r.preview_url, r.url, r.width, r.height))
                        .collect()
                }
                Err(e) => {
                    tracing::error!("GIF search failed for '{}': {}", query, e);
                    Vec::new()
                }
            }
        }
        None => {
            tracing::debug!("GIF client not available - search not possible");
            Vec::new()
        }
    }
}

fn wire_mention_triggered(window: &MainWindow, grpc_pool: Arc<GrpcClientPool>, chat_state: ChatState) {
    let weak_window = window.as_weak();

    window.on_mention_triggered(move |query: SharedString| {
        let query_str = query.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let grpc_pool = grpc_pool.clone();

        if query_str.is_empty() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_mention_autocomplete_visible(false);
                }
            });
            return;
        }

        tracing::debug!("Mention triggered with query: {}", query_str);

        // Spawn async task to fetch channel members
        tokio::spawn(async move {
            // Fetch raw suggestion data (Send-safe)
            let raw_suggestions = fetch_mention_suggestions_raw(&query_str, &grpc_pool, &chat_state).await;

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if raw_suggestions.is_empty() {
                        w.set_mention_autocomplete_visible(false);
                    } else {
                        // Convert to Slint types inside the UI thread
                        let suggestions: Vec<MentionSuggestion> = raw_suggestions
                            .into_iter()
                            .map(|(id, name, mention_type, has_avatar)| MentionSuggestion {
                                id: id.into(),
                                name: name.into(),
                                mention_type: mention_type.into(),
                                avatar: slint::Image::default(),
                                has_avatar,
                            })
                            .collect();

                        w.set_mention_suggestions(slint::ModelRc::new(slint::VecModel::from(suggestions)));
                        w.set_mention_selected_index(0);
                        w.set_mention_autocomplete_visible(true);
                        // Close other pickers
                        w.set_composer_emoji_picker_visible(false);
                        w.set_gif_picker_visible(false);
                    }
                }
            });
        });
    });
}

fn wire_mention_selected(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_mention_suggestion_selected(move |_id: SharedString, display: SharedString| {
        let weak = weak_window.clone();
        let display_str = display.to_string();
        tracing::debug!("Mention selected: {}", display_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Get current composer text
                let current_text = w.get_composer_text().to_string();

                // Find the last @ and replace from there
                if let Some(at_pos) = current_text.rfind('@') {
                    let new_text = format!("{}{} ", &current_text[..at_pos], display_str);
                    w.set_composer_text(new_text.into());
                }

                w.set_mention_autocomplete_visible(false);
            }
        });
    });
}

/// Raw mention suggestion data (Send-safe, no Slint types).
type RawMentionSuggestion = (String, String, String, bool); // (id, name, mention_type, has_avatar)

/// Fetch mention suggestions based on query (async version that fetches from server).
/// Returns raw data that can be safely sent across threads.
async fn fetch_mention_suggestions_raw(
    query: &str,
    grpc_pool: &GrpcClientPool,
    chat_state: &ChatState,
) -> Vec<RawMentionSuggestion> {
    let query_lower = query.to_lowercase();
    let mut suggestions: Vec<RawMentionSuggestion> = Vec::new();

    // Add special mentions first
    if "here".starts_with(&query_lower) {
        suggestions.push((
            "here".to_string(),
            "@here".to_string(),
            "here".to_string(),
            false,
        ));
    }
    if "all".starts_with(&query_lower) || "everyone".starts_with(&query_lower) {
        suggestions.push((
            "all".to_string(),
            "@all".to_string(),
            "all".to_string(),
            false,
        ));
    }

    // Get current channel ID
    let channel_id = match chat_state.get().active_channel_id.clone() {
        Some(id) => id,
        None => {
            tracing::debug!("No active channel for mention search");
            return suggestions;
        }
    };

    // Fetch channel members
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    match channel_client
        .get_channel_members(&channel_id.to_string(), 50, 0)
        .await
    {
        Ok(response) => {
            // For each member, fetch their profile to get display name
            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            for member in response.members.iter().take(20) {
                // Fetch user profile
                match user_client.get_user_profile(&member.user_id).await {
                    Ok(profile) => {
                        let display_name = if profile.display_name.is_empty() {
                            // Use user_id as fallback if no display name
                            member.user_id.clone()
                        } else {
                            profile.display_name.clone()
                        };

                        let name_lower = display_name.to_lowercase();

                        // Check if query matches display name or user_id
                        if name_lower.contains(&query_lower)
                            || member.user_id.to_lowercase().contains(&query_lower)
                        {
                            suggestions.push((
                                member.user_id.clone(),
                                format!("@{}", display_name),
                                "user".to_string(),
                                !profile.avatar_url.is_empty(),
                            ));
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            "Failed to fetch profile for user {}: {}",
                            member.user_id,
                            e
                        );
                    }
                }

                // Limit fetches to avoid too many requests
                if suggestions.len() >= 8 {
                    break;
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to fetch channel members for mentions: {}", e);
        }
    }

    // Limit to 8 suggestions
    suggestions.truncate(8);
    suggestions
}
