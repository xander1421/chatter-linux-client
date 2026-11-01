/// Sampling parameters for text generation.
#[derive(Debug, Clone)]
pub struct SampleParams {
    pub max_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: usize,
    pub repeat_penalty: f32,
    /// Skip thinking-model reasoning (structured tasks).
    pub disable_thinking: bool,
    /// Constrain output to a JSON schema (HTTP providers).
    pub json_schema: Option<serde_json::Value>,
}

impl Default for SampleParams {
    fn default() -> Self {
        Self {
            max_tokens: 512,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.1,
            disable_thinking: false,
            json_schema: None,
        }
    }
}
