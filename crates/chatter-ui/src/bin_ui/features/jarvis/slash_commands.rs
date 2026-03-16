//! Jarvis slash command parsing.
//!
//! Parses Jarvis-specific slash commands from message text.

/// Jarvis slash commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JarvisCommand {
    /// /ask <query> - Ask Jarvis a question.
    Ask(String),
    /// /summarize - Summarize current conversation.
    Summarize,
    /// /knowledge <entity> - Get knowledge about an entity.
    Knowledge(String),
    /// /remember <fact> - Store a fact in memory.
    Remember(String),
}

/// Parse a slash command from message text.
///
/// Returns None if not a recognized Jarvis command.
pub fn parse_slash_command(text: &str) -> Option<JarvisCommand> {
    let text = text.trim();

    if let Some(rest) = text.strip_prefix("/ask ") {
        let query = rest.trim();
        if !query.is_empty() {
            return Some(JarvisCommand::Ask(query.to_string()));
        }
    }

    if text == "/summarize" || text.starts_with("/summarize ") {
        return Some(JarvisCommand::Summarize);
    }

    if let Some(rest) = text.strip_prefix("/knowledge ") {
        let entity = rest.trim();
        if !entity.is_empty() {
            return Some(JarvisCommand::Knowledge(entity.to_string()));
        }
    }

    if let Some(rest) = text.strip_prefix("/remember ") {
        let fact = rest.trim();
        if !fact.is_empty() {
            return Some(JarvisCommand::Remember(fact.to_string()));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ask() {
        assert_eq!(
            parse_slash_command("/ask what is the weather?"),
            Some(JarvisCommand::Ask("what is the weather?".to_string()))
        );
        assert_eq!(
            parse_slash_command("/ask   spaced query  "),
            Some(JarvisCommand::Ask("spaced query".to_string()))
        );
        // Empty query should not match
        assert_eq!(parse_slash_command("/ask "), None);
        assert_eq!(parse_slash_command("/ask"), None);
    }

    #[test]
    fn test_parse_summarize() {
        assert_eq!(
            parse_slash_command("/summarize"),
            Some(JarvisCommand::Summarize)
        );
        assert_eq!(
            parse_slash_command("/summarize this channel"),
            Some(JarvisCommand::Summarize)
        );
        assert_eq!(
            parse_slash_command("  /summarize  "),
            Some(JarvisCommand::Summarize)
        );
    }

    #[test]
    fn test_parse_knowledge() {
        assert_eq!(
            parse_slash_command("/knowledge Alice"),
            Some(JarvisCommand::Knowledge("Alice".to_string()))
        );
        assert_eq!(
            parse_slash_command("/knowledge  Project X  "),
            Some(JarvisCommand::Knowledge("Project X".to_string()))
        );
        // Empty entity should not match
        assert_eq!(parse_slash_command("/knowledge "), None);
        assert_eq!(parse_slash_command("/knowledge"), None);
    }

    #[test]
    fn test_parse_remember() {
        assert_eq!(
            parse_slash_command("/remember Alice works on Rust"),
            Some(JarvisCommand::Remember("Alice works on Rust".to_string()))
        );
        // Empty fact should not match
        assert_eq!(parse_slash_command("/remember "), None);
        assert_eq!(parse_slash_command("/remember"), None);
    }

    #[test]
    fn test_not_jarvis_command() {
        assert_eq!(parse_slash_command("/other command"), None);
        assert_eq!(parse_slash_command("regular message"), None);
        assert_eq!(parse_slash_command("/help"), None);
        assert_eq!(parse_slash_command("@jarvis help"), None);
    }

    #[test]
    fn test_case_sensitivity() {
        // Commands are case-sensitive
        assert_eq!(parse_slash_command("/ASK hello"), None);
        assert_eq!(parse_slash_command("/Summarize"), None);
    }
}
