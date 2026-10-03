//! Hardware profiling for model-fit suggestions.
//!
//! Reads arch/cores from std, RAM from /proc/meminfo, GPU VRAM by shelling
//! out to `nvidia-smi` (and best-effort `rocm-smi`). No extra crates — the
//! catalog only needs coarse numbers to tag models as GPU / CPU / too big.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// GPU vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuVendor {
    Nvidia,
    Amd,
}

/// A detected GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    pub name: String,
    pub vram_gb: f64,
}

/// Coarse hardware profile of this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareProfile {
    /// CPU architecture (`x86_64`, `aarch64`, …).
    pub arch: &'static str,
    pub cpu_cores: usize,
    pub ram_gb: f64,
    pub gpus: Vec<GpuInfo>,
}

/// How well a model fits this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelFit {
    /// Fits in GPU VRAM — fast.
    Gpu,
    /// Fits in system RAM — runs on CPU, slow.
    CpuSlow,
    /// Exceeds both VRAM and RAM.
    TooBig,
}

/// Leave headroom for OS / other processes / KV cache.
const FIT_HEADROOM: f64 = 0.9;

impl HardwareProfile {
    /// Detect the current machine. Never fails — missing sources degrade to
    /// zeros/empty (a 0 GB RAM reading makes everything TooBig, which is the
    /// honest answer when we can't see the hardware).
    pub fn detect() -> Self {
        Self {
            arch: std::env::consts::ARCH,
            cpu_cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            ram_gb: read_meminfo_total_gb(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default()),
            gpus: detect_gpus(),
        }
    }

    /// Best available VRAM across GPUs.
    pub fn max_vram_gb(&self) -> f64 {
        self.gpus.iter().map(|g| g.vram_gb).fold(0.0, f64::max)
    }

    /// Classify a model by its approximate in-memory size.
    pub fn fit(&self, model_size_gb: f64) -> ModelFit {
        if model_size_gb <= self.max_vram_gb() * FIT_HEADROOM {
            ModelFit::Gpu
        } else if model_size_gb <= self.ram_gb * FIT_HEADROOM {
            ModelFit::CpuSlow
        } else {
            ModelFit::TooBig
        }
    }

    /// One-line summary for the settings UI, e.g.
    /// `x86_64 · 16 cores · 31.3 GB RAM · NVIDIA GeForce RTX 4070 (12.0 GB)`.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} · {} cores · {:.1} GB RAM",
            self.arch, self.cpu_cores, self.ram_gb
        );
        for gpu in &self.gpus {
            s.push_str(&format!(" · {} ({:.1} GB)", gpu.name, gpu.vram_gb));
        }
        s
    }
}

/// Parse `MemTotal: 32762524 kB` out of /proc/meminfo content.
fn read_meminfo_total_gb(meminfo: &str) -> f64 {
    meminfo
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("MemTotal:")?;
            let kb: f64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
            Some(kb / 1024.0 / 1024.0)
        })
        .unwrap_or(0.0)
}

fn detect_gpus() -> Vec<GpuInfo> {
    let mut gpus = detect_nvidia();
    gpus.extend(detect_amd());
    gpus
}

fn detect_nvidia() -> Vec<GpuInfo> {
    let Ok(output) = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_nvidia_smi_csv(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `NVIDIA GeForce RTX 4070, 12282` lines (MiB with nounits).
fn parse_nvidia_smi_csv(csv: &str) -> Vec<GpuInfo> {
    csv.lines()
        .filter_map(|line| {
            let (name, mib) = line.rsplit_once(',')?;
            let mib: f64 = mib.trim().parse().ok()?;
            Some(GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: name.trim().to_string(),
                vram_gb: mib / 1024.0,
            })
        })
        .collect()
}

// best-effort AMD probe — silent skip when rocm-smi absent or its
// output shape differs. NVIDIA path is the tested one.
fn detect_amd() -> Vec<GpuInfo> {
    let Ok(output) = Command::new("rocm-smi")
        .args(["--showmeminfo", "vram", "--csv"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_rocm_smi_csv(&String::from_utf8_lossy(&output.stdout))
}

/// Parse rocm-smi CSV: `device,VRAM Total Memory (B),VRAM Total Used Memory (B)`
/// then `card0,17163091968,...` rows.
fn parse_rocm_smi_csv(csv: &str) -> Vec<GpuInfo> {
    csv.lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split(',');
            let device = parts.next()?.trim();
            if !device.starts_with("card") {
                return None;
            }
            let bytes: f64 = parts.next()?.trim().parse().ok()?;
            Some(GpuInfo {
                vendor: GpuVendor::Amd,
                name: format!("AMD {}", device),
                vram_gb: bytes / 1024.0 / 1024.0 / 1024.0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses() {
        let meminfo = "MemTotal:       32762524 kB\nMemFree:        20000000 kB\n";
        let gb = read_meminfo_total_gb(meminfo);
        assert!((gb - 31.24).abs() < 0.1, "got {gb}");
    }

    #[test]
    fn meminfo_missing_is_zero() {
        assert_eq!(read_meminfo_total_gb(""), 0.0);
    }

    #[test]
    fn nvidia_csv_parses() {
        let csv = "NVIDIA GeForce RTX 4070, 12282\nNVIDIA RTX A6000, 49140\n";
        let gpus = parse_nvidia_smi_csv(csv);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4070");
        assert!((gpus[0].vram_gb - 11.99).abs() < 0.1);
        assert_eq!(gpus[1].vendor, GpuVendor::Nvidia);
    }

    #[test]
    fn nvidia_csv_garbage_skipped() {
        assert!(parse_nvidia_smi_csv("no gpus here\n").is_empty());
    }

    #[test]
    fn rocm_csv_parses() {
        let csv = "device,VRAM Total Memory (B),VRAM Total Used Memory (B)\ncard0,17163091968,1000000\n";
        let gpus = parse_rocm_smi_csv(csv);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor, GpuVendor::Amd);
        assert!((gpus[0].vram_gb - 15.98).abs() < 0.1);
    }

    #[test]
    fn fit_heuristic() {
        let profile = HardwareProfile {
            arch: "x86_64",
            cpu_cores: 16,
            ram_gb: 32.0,
            gpus: vec![GpuInfo {
                vendor: GpuVendor::Nvidia,
                name: "RTX 4070".into(),
                vram_gb: 12.0,
            }],
        };
        assert_eq!(profile.fit(5.0), ModelFit::Gpu); // ≤ 10.8 VRAM budget
        assert_eq!(profile.fit(11.0), ModelFit::CpuSlow); // ≤ 28.8 RAM budget
        assert_eq!(profile.fit(30.0), ModelFit::TooBig);
    }

    #[test]
    fn fit_no_gpu_no_ram_is_too_big() {
        let profile = HardwareProfile {
            arch: "x86_64",
            cpu_cores: 4,
            ram_gb: 0.0,
            gpus: vec![],
        };
        assert_eq!(profile.fit(1.0), ModelFit::TooBig);
    }

    #[test]
    fn detect_never_panics() {
        let profile = HardwareProfile::detect();
        assert!(!profile.arch.is_empty());
        assert!(profile.cpu_cores >= 1);
        let _ = profile.summary();
    }
}
