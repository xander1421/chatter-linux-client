use super::Migration;

pub struct V120SecretReceiveArtifacts;

impl Migration for V120SecretReceiveArtifacts {
    fn version(&self) -> u32 {
        120
    }

    fn description(&self) -> &'static str {
        "Retain authenticated secret data posts before acknowledgment"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE reaction_receive_artifacts ADD COLUMN kind TEXT NOT NULL DEFAULT 'reaction' CHECK(kind IN ('reaction','content'));
         ALTER TABLE reaction_receive_artifacts ADD COLUMN upstream_acked_at INTEGER;"
    }
}
