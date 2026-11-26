use super::Migration;

pub struct V148GifCredentials;

impl Migration for V148GifCredentials {
    fn version(&self) -> u32 {
        148
    }

    fn description(&self) -> &'static str {
        "Separate the GIF credential pool"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE mailbox_tokens RENAME COLUMN attachment TO purpose;"
    }
}
