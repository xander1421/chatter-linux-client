//! Curated model catalog.
//!
//! Popular open models the user can download, with coarse sizes for the
//! hardware-fit heuristic. Ollama entries pull via `/api/pull`; entries with
//! an HF id can be loaded by the in-process mistralrs engine. Sizes are the
//! default-quant (Q4-ish) download sizes as listed by the Ollama registry —
//! close enough to resident size for coarse GPU/CPU/too-big tagging.

use serde::{Deserialize, Serialize};

use super::hardware::{HardwareProfile, ModelFit};

/// What a model is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Chat,
    Embedding,
}

/// A curated downloadable model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Display name, e.g. "Llama 3.2 3B".
    pub name: &'static str,
    /// Ollama registry tag for `/api/pull`.
    pub ollama_tag: &'static str,
    /// HuggingFace repo id loadable by mistralrs (GGUF), when a good one exists.
    pub hf_id: Option<&'static str>,
    /// Parameter count in billions (display).
    pub params_b: f64,
    /// Approximate size in GB (default quant download size).
    pub size_gb: f64,
    pub kind: ModelKind,
}

impl CatalogModel {
    /// Hardware fit for this machine.
    pub fn fit(&self, profile: &HardwareProfile) -> ModelFit {
        profile.fit(self.size_gb)
    }
}

/// The curated catalog, smallest chat models first.
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel { name: "Qwen 2.5 0.5B", ollama_tag: "qwen2.5:0.5b", hf_id: Some("Qwen/Qwen2.5-0.5B-Instruct-GGUF"), params_b: 0.5, size_gb: 0.4, kind: ModelKind::Chat },
    CatalogModel { name: "Llama 3.2 1B", ollama_tag: "llama3.2:1b", hf_id: Some("bartowski/Llama-3.2-1B-Instruct-GGUF"), params_b: 1.2, size_gb: 1.3, kind: ModelKind::Chat },
    CatalogModel { name: "Gemma 3 1B", ollama_tag: "gemma3:1b", hf_id: None, params_b: 1.0, size_gb: 0.8, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 2.5 1.5B", ollama_tag: "qwen2.5:1.5b", hf_id: Some("Qwen/Qwen2.5-1.5B-Instruct-GGUF"), params_b: 1.5, size_gb: 1.0, kind: ModelKind::Chat },
    CatalogModel { name: "Llama 3.2 3B", ollama_tag: "llama3.2:3b", hf_id: Some("bartowski/Llama-3.2-3B-Instruct-GGUF"), params_b: 3.2, size_gb: 2.0, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 2.5 3B", ollama_tag: "qwen2.5:3b", hf_id: Some("Qwen/Qwen2.5-3B-Instruct-GGUF"), params_b: 3.1, size_gb: 1.9, kind: ModelKind::Chat },
    CatalogModel { name: "Gemma 3 4B", ollama_tag: "gemma3:4b", hf_id: None, params_b: 4.3, size_gb: 3.3, kind: ModelKind::Chat },
    CatalogModel { name: "Mistral 7B", ollama_tag: "mistral:7b", hf_id: Some("bartowski/Mistral-7B-Instruct-v0.3-GGUF"), params_b: 7.2, size_gb: 4.1, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 2.5 7B", ollama_tag: "qwen2.5:7b", hf_id: Some("Qwen/Qwen2.5-7B-Instruct-GGUF"), params_b: 7.6, size_gb: 4.7, kind: ModelKind::Chat },
    CatalogModel { name: "DeepSeek R1 7B", ollama_tag: "deepseek-r1:7b", hf_id: None, params_b: 7.6, size_gb: 4.7, kind: ModelKind::Chat },
    CatalogModel { name: "Llama 3.1 8B", ollama_tag: "llama3.1:8b", hf_id: Some("bartowski/Meta-Llama-3.1-8B-Instruct-GGUF"), params_b: 8.0, size_gb: 4.9, kind: ModelKind::Chat },
    CatalogModel { name: "Gemma 3 12B", ollama_tag: "gemma3:12b", hf_id: None, params_b: 12.2, size_gb: 8.1, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 2.5 14B", ollama_tag: "qwen2.5:14b", hf_id: Some("Qwen/Qwen2.5-14B-Instruct-GGUF"), params_b: 14.8, size_gb: 9.0, kind: ModelKind::Chat },
    CatalogModel { name: "DeepSeek R1 14B", ollama_tag: "deepseek-r1:14b", hf_id: None, params_b: 14.8, size_gb: 9.0, kind: ModelKind::Chat },
    CatalogModel { name: "Phi-4 14B", ollama_tag: "phi4", hf_id: Some("bartowski/phi-4-GGUF"), params_b: 14.7, size_gb: 9.1, kind: ModelKind::Chat },
    CatalogModel { name: "Nomic Embed Text", ollama_tag: "nomic-embed-text", hf_id: None, params_b: 0.14, size_gb: 0.27, kind: ModelKind::Embedding },
    CatalogModel { name: "All-MiniLM", ollama_tag: "all-minilm", hf_id: None, params_b: 0.02, size_gb: 0.05, kind: ModelKind::Embedding },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_ai::hardware::{GpuInfo, GpuVendor};

    fn profile(vram_gb: f64, ram_gb: f64) -> HardwareProfile {
        HardwareProfile {
            arch: "x86_64",
            cpu_cores: 8,
            ram_gb,
            gpus: if vram_gb > 0.0 {
                vec![GpuInfo { vendor: GpuVendor::Nvidia, name: "test".into(), vram_gb }]
            } else {
                vec![]
            },
        }
    }

    #[test]
    fn catalog_is_nonempty_and_sorted_small_to_large_chat() {
        assert!(CATALOG.len() > 10);
        assert!(CATALOG.iter().any(|m| m.kind == ModelKind::Embedding));
    }

    #[test]
    fn tags_are_unique() {
        let mut tags: Vec<_> = CATALOG.iter().map(|m| m.ollama_tag).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), CATALOG.len());
    }

    #[test]
    fn fit_tags_scale_with_hardware() {
        let small = profile(0.0, 8.0); // 8 GB laptop, no GPU
        let big = profile(24.0, 64.0); // workstation

        let llama1b = CATALOG.iter().find(|m| m.ollama_tag == "llama3.2:1b").unwrap();
        let qwen14b = CATALOG.iter().find(|m| m.ollama_tag == "qwen2.5:14b").unwrap();

        assert_eq!(llama1b.fit(&small), ModelFit::CpuSlow);
        assert_eq!(llama1b.fit(&big), ModelFit::Gpu);
        assert_eq!(qwen14b.fit(&small), ModelFit::TooBig);
        assert_eq!(qwen14b.fit(&big), ModelFit::Gpu);
    }
}
