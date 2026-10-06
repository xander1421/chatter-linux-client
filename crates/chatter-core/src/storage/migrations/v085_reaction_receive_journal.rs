use super::Migration;
pub struct V085ReactionReceiveJournal;
impl Migration for V085ReactionReceiveJournal {
    fn version(&self) -> u32 {
        85
    }
    fn description(&self) -> &'static str {
        "Atomic authenticated reaction receive journal"
    }
    fn up(&self) -> &'static str {
        r#"
 CREATE TABLE reaction_receive_artifacts (
 id TEXT PRIMARY KEY, local_account TEXT NOT NULL,local_device INTEGER NOT NULL CHECK(local_device BETWEEN 1 AND 127),
 transport TEXT NOT NULL CHECK(transport IN ('anon_dm','anon_group')),scope TEXT NOT NULL CHECK(length(scope) BETWEEN 1 AND 256),
 envelope_id TEXT NOT NULL CHECK(length(envelope_id) BETWEEN 1 AND 256),server_timestamp_ns INTEGER NOT NULL CHECK(server_timestamp_ns>0),
 digest BLOB NOT NULL CHECK(length(digest)=32),sender TEXT NOT NULL,sender_device INTEGER NOT NULL CHECK(sender_device BETWEEN 1 AND 127),
 content BLOB NOT NULL,state TEXT NOT NULL CHECK(state IN ('pending','applied','acknowledged')),acknowledged_at INTEGER,
 message_type INTEGER NOT NULL CHECK(message_type IN (2,3)),
 CHECK ((state='acknowledged')=(acknowledged_at IS NOT NULL)),
 CHECK ((state='acknowledged' AND length(content)=0) OR (state!='acknowledged' AND length(content) BETWEEN 1 AND 65536)),
 UNIQUE(local_account,local_device,transport,scope,envelope_id));
 CREATE INDEX idx_reaction_receive_pending ON reaction_receive_artifacts(local_account,local_device,server_timestamp_ns,id) WHERE state='pending';
 "#
    }
}
