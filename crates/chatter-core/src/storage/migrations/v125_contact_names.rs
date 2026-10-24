use super::Migration;

pub struct V125ContactNames;

impl Migration for V125ContactNames {
    fn version(&self) -> u32 { 125 }
    fn description(&self) -> &'static str { "Store device-local contact names per account" }
    fn up(&self) -> &'static str {
        "CREATE TABLE contact_names (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            name TEXT NOT NULL,
            PRIMARY KEY (owner_id, peer_id)
        );"
    }
}
