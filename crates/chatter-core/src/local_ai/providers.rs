//! Local AI provider detection.
//!
//! Probes the well-known local inference servers on their default ports and
//! reports which are running plus the models each has installed. mistralrs
//! (in-process GPU inference) is reported from compile-time feature state.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, GenerateParams, LocalAIBackend,
};
use super::ollama::OllamaBackend;
use super::openai_compat::OpenAiCompatBackend;
use super::settings::LocalAISettings;

/// Timeout for a single provider probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A known local AI provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Ollama — native API, supports pull/delete.
    Ollama,
    /// LM Studio — OpenAI-compatible.
    LmStudio,
    /// llama.cpp server — OpenAI-compatible.
    LlamaCpp,
    /// vLLM — OpenAI-compatible.
    Vllm,
    /// mistralrs in-process engine (compiled with the `local-ai` feature).
    MistralRs,
}

impl ProviderKind {
    /// All HTTP-probeable kinds (excludes in-process mistralrs).
    pub const HTTP_KINDS: [ProviderKind; 4] = [
        ProviderKind::Ollama,
        ProviderKind::LmStudio,
        ProviderKind::LlamaCpp,
        ProviderKind::Vllm,
    ];

    /// Default base URL for HTTP providers; `None` for in-process.
    pub fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("http://localhost:11434"),
            Self::LmStudio => Some("http://localhost:1234"),
            Self::LlamaCpp => Some("http://localhost:8080"),
            Self::Vllm => Some("http://localhost:8000"),
            Self::MistralRs => None,
        }
    }

    /// Human-readable name for the UI.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vLLM",
            Self::MistralRs => "Built-in (GPU)",
        }
    }

    /// Stable identifier for settings persistence.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LmStudio => "lm-studio",
            Self::LlamaCpp => "llama-cpp",
            Self::Vllm => "vllm",
            Self::MistralRs => "mistralrs",
        }
    }

    /// Parse a settings identifier back to a kind.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ollama" => Some(Self::Ollama),
            "lm-studio" => Some(Self::LmStudio),
            "llama-cpp" => Some(Self::LlamaCpp),
            "vllm" => Some(Self::Vllm),
            "mistralrs" => Some(Self::MistralRs),
            _ => None,
        }
    }

    /// Whether this provider can download models itself (catalog "Download"
    /// button). Only Ollama has a pull API; mistralrs fetches from HF by id.
    pub fn supports_pull(self) -> bool {
        matches!(self, Self::Ollama | Self::MistralRs)
    }
}

/// A provider found running on this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedProvider {
    pub kind: ProviderKind,
    /// Base URL (empty for in-process mistralrs).
    pub url: String,
    /// Models installed on the provider (empty for mistralrs — it loads by id).
    pub models: Vec<AvailableModel>,
}

/// Whether the in-process mistralrs engine was compiled in.
pub fn mistralrs_available() -> bool {
    cfg!(feature = "local-ai")
}

/// Probe all known providers concurrently.
///
/// `ollama_url` honors the user's configured override; other providers use
/// their default ports. Unreachable providers are simply absent from the
/// result — a failed probe is not an error.
pub async fn detect_providers(ollama_url: &str) -> Vec<DetectedProvider> {
    let ollama = probe_ollama(ollama_url);
    let lm_studio = probe_openai_compat(ProviderKind::LmStudio);
    let llama_cpp = probe_openai_compat(ProviderKind::LlamaCpp);
    let vllm = probe_openai_compat(ProviderKind::Vllm);

    let (ollama, lm_studio, llama_cpp, vllm) = tokio::join!(ollama, lm_studio, llama_cpp, vllm);

    let mut detected: Vec<DetectedProvider> =
        [ollama, lm_studio, llama_cpp, vllm].into_iter().flatten().collect();

    if mistralrs_available() {
        detected.push(DetectedProvider {
            kind: ProviderKind::MistralRs,
            url: String::new(),
            models: Vec::new(),
        });
    }

    detected
}

async fn probe_ollama(url: &str) -> Option<DetectedProvider> {
    let backend = OllamaBackend::new(url, PROBE_TIMEOUT);
    let models = backend.list_models().await.ok()?;
    Some(DetectedProvider {
        kind: ProviderKind::Ollama,
        url: url.trim_end_matches('/').to_string(),
        models,
    })
}

async fn probe_openai_compat(kind: ProviderKind) -> Option<DetectedProvider> {
    let url = kind.default_url()?;
    let backend = OpenAiCompatBackend::new(url, PROBE_TIMEOUT, kind.as_str());
    let models = backend.list_models().await.ok()?;
    Some(DetectedProvider {
        kind,
        url: url.to_string(),
        models,
    })
}

// ==================== Enum dispatch over backends ====================

/// Concrete backend for any HTTP provider, dispatched by enum (the
/// `LocalAIBackend` trait uses native `async fn` and is not dyn-safe).
pub enum AnyBackend {
    Ollama(OllamaBackend),
    OpenAiCompat(OpenAiCompatBackend),
}

impl AnyBackend {
    /// Build the backend for an HTTP provider kind. Returns `None` for
    /// `MistralRs` — that engine is driven through `LocalAIActorHandle`,
    /// not this trait.
    pub fn for_kind(kind: ProviderKind, settings: &LocalAISettings) -> Option<Self> {
        match kind {
            ProviderKind::Ollama => Some(Self::Ollama(OllamaBackend::new(
                &settings.ollama_url,
                settings.request_timeout,
            ))),
            ProviderKind::LmStudio | ProviderKind::LlamaCpp | ProviderKind::Vllm => {
                Some(Self::OpenAiCompat(OpenAiCompatBackend::new(
                    kind.default_url().expect("http provider has default url"),
                    settings.request_timeout,
                    kind.as_str(),
                )))
            }
            ProviderKind::MistralRs => None,
        }
    }
}

impl LocalAIBackend for AnyBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        match self {
            Self::Ollama(b) => b.health_check().await,
            Self::OpenAiCompat(b) => b.health_check().await,
        }
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        match self {
            Self::Ollama(b) => b.chat(model, messages, params).await,
            Self::OpenAiCompat(b) => b.chat(model, messages, params).await,
        }
    }

    async fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        match self {
            Self::Ollama(b) => b.embed(model, texts).await,
            Self::OpenAiCompat(b) => b.embed(model, texts).await,
        }
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        match self {
            Self::Ollama(b) => b.list_models().await,
            Self::OpenAiCompat(b) => b.list_models().await,
        }
    }

    async fn pull_model(&self, name: &str) -> Result<(), BackendError> {
        match self {
            Self::Ollama(b) => b.pull_model(name).await,
            Self::OpenAiCompat(b) => b.pull_model(name).await,
        }
    }

    async fn delete_model(&self, name: &str) -> Result<(), BackendError> {
        match self {
            Self::Ollama(b) => b.delete_model(name).await,
            Self::OpenAiCompat(b) => b.delete_model(name).await,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(b) => b.name(),
            Self::OpenAiCompat(b) => b.name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrips_through_settings_string() {
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::LmStudio,
            ProviderKind::LlamaCpp,
            ProviderKind::Vllm,
            ProviderKind::MistralRs,
        ] {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProviderKind::parse("nonsense"), None);
    }

    #[test]
    fn http_kinds_have_urls_mistralrs_does_not() {
        for kind in ProviderKind::HTTP_KINDS {
            assert!(kind.default_url().is_some());
        }
        assert!(ProviderKind::MistralRs.default_url().is_none());
    }

    #[test]
    fn any_backend_construction() {
        let settings = LocalAISettings::default();
        assert!(AnyBackend::for_kind(ProviderKind::Ollama, &settings).is_some());
        assert!(AnyBackend::for_kind(ProviderKind::LmStudio, &settings).is_some());
        assert!(AnyBackend::for_kind(ProviderKind::MistralRs, &settings).is_none());
    }

    #[tokio::test]
    async fn detect_providers_skips_unreachable() {
        // Port 1 is never an AI server; probe must resolve to absent, not error.
        let detected = detect_providers("http://127.0.0.1:1").await;
        let has_ollama = detected.iter().any(|p| p.kind == ProviderKind::Ollama);
        assert!(!has_ollama);
    }

    /// Live check against a locally running Ollama. Run with:
    /// `cargo test -p chatter-core --lib live_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running Ollama on localhost:11434"]
    async fn live_detect_finds_local_ollama() {
        let detected = detect_providers("http://localhost:11434").await;
        let ollama = detected.iter().find(|p| p.kind == ProviderKind::Ollama);
        assert!(ollama.is_some(), "Ollama not detected — is it running?");
        println!("detected: {:?}", detected.iter().map(|p| (p.kind, p.models.len())).collect::<Vec<_>>());
    }
}
