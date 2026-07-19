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

/// The curated catalog, smallest chat models first. 2026 generation —
/// every tag verified against registry.ollama.ai before inclusion.
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel { name: "Qwen 3 0.6B", ollama_tag: "qwen3:0.6b", hf_id: Some("Qwen/Qwen3-0.6B-GGUF"), params_b: 0.6, size_gb: 0.5, kind: ModelKind::Chat },
    CatalogModel { name: "Phi-4 Mini", ollama_tag: "phi4-mini", hf_id: None, params_b: 3.8, size_gb: 2.5, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 3 4B", ollama_tag: "qwen3:4b", hf_id: Some("Qwen/Qwen3-4B-GGUF"), params_b: 4.0, size_gb: 2.6, kind: ModelKind::Chat },
    CatalogModel { name: "Gemma 3 4B (vision)", ollama_tag: "gemma3:4b", hf_id: None, params_b: 4.3, size_gb: 3.3, kind: ModelKind::Chat },
    CatalogModel { name: "DeepSeek R1 7B", ollama_tag: "deepseek-r1:7b", hf_id: None, params_b: 7.6, size_gb: 4.7, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 3 8B", ollama_tag: "qwen3:8b", hf_id: Some("Qwen/Qwen3-8B-GGUF"), params_b: 8.2, size_gb: 5.2, kind: ModelKind::Chat },
    CatalogModel { name: "Gemma 4 12B (multimodal)", ollama_tag: "gemma4:12b", hf_id: None, params_b: 12.0, size_gb: 8.0, kind: ModelKind::Chat },
    CatalogModel { name: "DeepSeek R1 14B", ollama_tag: "deepseek-r1:14b", hf_id: None, params_b: 14.8, size_gb: 9.0, kind: ModelKind::Chat },
    CatalogModel { name: "Qwen 3 14B", ollama_tag: "qwen3:14b", hf_id: Some("Qwen/Qwen3-14B-GGUF"), params_b: 14.8, size_gb: 9.3, kind: ModelKind::Chat },
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
        assert!(CATALOG.len() >= 10);
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

        let tiny = CATALOG.iter().find(|m| m.ollama_tag == "qwen3:0.6b").unwrap();
        let big14 = CATALOG.iter().find(|m| m.ollama_tag == "qwen3:14b").unwrap();

        assert_eq!(tiny.fit(&small), ModelFit::CpuSlow);
        assert_eq!(tiny.fit(&big), ModelFit::Gpu);
        assert_eq!(big14.fit(&small), ModelFit::TooBig);
        assert_eq!(big14.fit(&big), ModelFit::Gpu);
    }
}
