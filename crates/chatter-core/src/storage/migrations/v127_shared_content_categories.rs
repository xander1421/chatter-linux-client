use super::Migration;

pub struct V127SharedContentCategories;

impl Migration for V127SharedContentCategories {
    fn version(&self) -> u32 { 127 }
    fn description(&self) -> &'static str { "Re-sort shared content stored under the retired media/music tabs" }
    fn up(&self) -> &'static str {
        // The category comes from MIME inside the bincode metadata, which SQL
        // cannot read; `unsorted` rows are resolved from it in Rust
        // (`shared_content_repo::resolve_unsorted`).
        "UPDATE shared_content SET category = 'unsorted' WHERE category IN ('media', 'music');"
    }
}
