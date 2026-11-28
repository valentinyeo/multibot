use serde::{Deserialize, Serialize};
use std::path::Path;

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_INTEL: u32 = 0x8086;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_MICROSOFT: u32 = 0x1414;

pub const ENV_GPU_MODE: &str = "MULTIBOT_GPU_MODE";
pub const ENV_GPU_LUID: &str = "MULTIBOT_GPU_LUID";
pub const ENV_GPU_REPORT_PATH: &str = "MULTIBOT_GPU_REPORT_PATH";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GpuPreference {
    /// Default: pick the lowest-power non-NVIDIA hardware GPU; if unavailable,
    /// fall back to Windows WARP software rendering rather than waking NVIDIA.
    #[default]
    AutoIntegrated,
    /// Force Microsoft Basic Render Driver / WARP software rendering.
    SoftwareOnly,
    /// Explicitly lock to a specific adapter LUID (formatted `HHHHHHHH:LLLLLLLL`).
    SpecificLuid { luid: String },
}

impl GpuPreference {
    pub fn env_mode_value(&self) -> &'static str {
        match self {
            Self::AutoIntegrated => "auto_integrated",
            Self::SoftwareOnly => "software_only",
            Self::SpecificLuid { .. } => "specific_luid",
        }
    }

    pub fn env_luid_value(&self) -> Option<&str> {
        match self {
            Self::SpecificLuid { luid } => Some(luid.as_str()),
            _ => None,
        }
    }

    pub fn from_cli_arg(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("auto") || trimmed.eq_ignore_ascii_case("integrated") {
            Self::AutoIntegrated
        } else if trimmed.eq_ignore_ascii_case("software") || trimmed.eq_ignore_ascii_case("warp") {
            Self::SoftwareOnly
        } else {
            Self::SpecificLuid {
                luid: trimmed.to_ascii_lowercase(),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuAdapterInfo {
    pub luid: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_vram_mb: u64,
    pub is_software: bool,
    pub name: String,
}

impl GpuAdapterInfo {
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id == VENDOR_NVIDIA
    }

    pub fn vendor_label(&self) -> &'static str {
        match self.vendor_id {
            VENDOR_INTEL => "Intel",
            VENDOR_AMD => "AMD",
            VENDOR_NVIDIA => "NVIDIA",
            VENDOR_MICROSOFT => "Microsoft WARP",
            _ => "Other",
        }
    }

    pub fn badge_label(&self) -> String {
        if self.is_software {
            format!("{} (Software)", self.name)
        } else if self.is_nvidia() {
            format!("{} (NVIDIA — Manual)", self.name)
        } else {
            format!("{} (iGPU · 0% NVIDIA)", self.name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuSelectionPlan {
    /// Ordered candidate indices into the input adapter slice.
    /// The DirectX layer tries these in order and NEVER touches adapters
    /// omitted from this list (so NVIDIA Optimus stays asleep).
    pub candidate_indices: Vec<usize>,
}

/// Computes the exact ordered list of DXGI adapters to attempt.
///
/// Hard guarantee: in `AutoIntegrated` and `SoftwareOnly` modes, NVIDIA
/// (`0x10DE`) adapters are NEVER included — not even as a fallback.
pub fn plan_adapter_order(adapters: &[GpuAdapterInfo], pref: &GpuPreference) -> GpuSelectionPlan {
    let mut order = Vec::new();

    match pref {
        GpuPreference::SpecificLuid { luid } => {
            if let Some(idx) = adapters
                .iter()
                .position(|a| a.luid.eq_ignore_ascii_case(luid))
            {
                order.push(idx);
            }
            // Safe fallback if the saved LUID disappeared (e.g., eGPU unplugged):
            // non-NVIDIA hardware first, then software — NEVER NVIDIA.
            for (i, a) in adapters.iter().enumerate() {
                if !order.contains(&i) && !a.is_software && !a.is_nvidia() {
                    order.push(i);
                }
            }
            for (i, a) in adapters.iter().enumerate() {
                if !order.contains(&i) && a.is_software {
                    order.push(i);
                }
            }
        }
        GpuPreference::AutoIntegrated => {
            for (i, a) in adapters.iter().enumerate() {
                if !a.is_software && !a.is_nvidia() {
                    order.push(i);
                }
            }
            for (i, a) in adapters.iter().enumerate() {
                if a.is_software {
                    order.push(i);
                }
            }
        }
        GpuPreference::SoftwareOnly => {
            for (i, a) in adapters.iter().enumerate() {
                if a.is_software {
                    order.push(i);
                }
            }
        }
    }

    GpuSelectionPlan {
        candidate_indices: order,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GpuRuntimeReport {
    pub mode: String,
    pub active: Option<GpuAdapterInfo>,
    pub discovered: Vec<GpuAdapterInfo>,
}

/// Parses the simple tab-separated report written by `patches/gpui-pre-windows`.
///
/// Format:
/// ```text
/// MODE\tauto_integrated
/// ACTIVE\t00000000:00012345\t8086\t9a49\t128\t0\tIntel(R) Iris(R) Xe Graphics
/// ADAPTER\t00000000:00012345\t8086\t9a49\t128\t0\tIntel(R) Iris(R) Xe Graphics
/// ADAPTER\t00000000:0006789a\t10de\t25a0\t4096\t0\tNVIDIA GeForce RTX 3050 Laptop GPU
/// ```
pub fn parse_gpu_report(content: &str) -> GpuRuntimeReport {
    let mut report = GpuRuntimeReport::default();
    for line in content.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.is_empty() {
            continue;
        }
        match parts[0] {
            "MODE" if parts.len() >= 2 => {
                report.mode = parts[1].to_string();
            }
            "ACTIVE" if parts.len() >= 7 => {
                if let Some(info) = parse_adapter_fields(&parts[1..]) {
                    report.active = Some(info);
                }
            }
            "ADAPTER" if parts.len() >= 7 => {
                if let Some(info) = parse_adapter_fields(&parts[1..]) {
                    report.discovered.push(info);
                }
            }
            _ => {}
        }
    }
    report
}

pub fn read_gpu_report_file(path: &Path) -> Option<GpuRuntimeReport> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_gpu_report(&text))
}

fn parse_adapter_fields(fields: &[&str]) -> Option<GpuAdapterInfo> {
    if fields.len() < 6 {
        return None;
    }
    let luid = fields[0].to_string();
    let vendor_id = u32::from_str_radix(fields[1].trim_start_matches("0x"), 16).ok()?;
    let device_id = u32::from_str_radix(fields[2].trim_start_matches("0x"), 16).ok()?;
    let dedicated_vram_mb = fields[3].parse::<u64>().ok()?;
    let is_software = fields[4] == "1" || fields[4].eq_ignore_ascii_case("true");
    let name = fields[5..].join("\t");
    Some(GpuAdapterInfo {
        luid,
        vendor_id,
        device_id,
        dedicated_vram_mb,
        is_software,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hybrid_laptop() -> Vec<GpuAdapterInfo> {
        // Deliberately put NVIDIA at index 0 to prove we never pick index 0 blindly.
        vec![
            GpuAdapterInfo {
                luid: "00000000:0000aaaa".into(),
                vendor_id: VENDOR_NVIDIA,
                device_id: 0x25A0,
                dedicated_vram_mb: 8192,
                is_software: false,
                name: "NVIDIA GeForce RTX 4060 Laptop GPU".into(),
            },
            GpuAdapterInfo {
                luid: "00000000:0000bbbb".into(),
                vendor_id: VENDOR_INTEL,
                device_id: 0xA7A0,
                dedicated_vram_mb: 128,
                is_software: false,
                name: "Intel(R) Iris(R) Xe Graphics".into(),
            },
            GpuAdapterInfo {
                luid: "00000000:0000cccc".into(),
                vendor_id: VENDOR_MICROSOFT,
                device_id: 0x008C,
                dedicated_vram_mb: 0,
                is_software: true,
                name: "Microsoft Basic Render Driver".into(),
            },
        ]
    }

    #[test]
    fn auto_integrated_skips_nvidia_even_when_adapter_zero() {
        let adapters = sample_hybrid_laptop();
        let plan = plan_adapter_order(&adapters, &GpuPreference::AutoIntegrated);
        assert_eq!(plan.candidate_indices, vec![1, 2]);
        for idx in &plan.candidate_indices {
            assert!(!adapters[*idx].is_nvidia(), "NVIDIA must never be selected");
        }
    }

    #[test]
    fn nvidia_only_machine_falls_back_to_warp_never_nvidia() {
        let adapters = vec![
            GpuAdapterInfo {
                luid: "00000000:0000aaaa".into(),
                vendor_id: VENDOR_NVIDIA,
                device_id: 0x25A0,
                dedicated_vram_mb: 8192,
                is_software: false,
                name: "NVIDIA GeForce RTX 4060".into(),
            },
            GpuAdapterInfo {
                luid: "00000000:0000cccc".into(),
                vendor_id: VENDOR_MICROSOFT,
                device_id: 0x008C,
                dedicated_vram_mb: 0,
                is_software: true,
                name: "Microsoft Basic Render Driver".into(),
            },
        ];
        let plan = plan_adapter_order(&adapters, &GpuPreference::AutoIntegrated);
        assert_eq!(plan.candidate_indices, vec![1]);
        assert!(adapters[plan.candidate_indices[0]].is_software);
    }

    #[test]
    fn explicit_luid_override_works_and_falls_back_safely() {
        let adapters = sample_hybrid_laptop();
        let pref = GpuPreference::SpecificLuid {
            luid: "00000000:0000bbbb".into(),
        };
        let plan = plan_adapter_order(&adapters, &pref);
        assert_eq!(plan.candidate_indices, vec![1, 2]);

        let missing = GpuPreference::SpecificLuid {
            luid: "deadbeef:00000000".into(),
        };
        let fallback_plan = plan_adapter_order(&adapters, &missing);
        assert_eq!(fallback_plan.candidate_indices, vec![1, 2]);
    }

    #[test]
    fn software_only_mode_picks_only_warp() {
        let adapters = sample_hybrid_laptop();
        let plan = plan_adapter_order(&adapters, &GpuPreference::SoftwareOnly);
        assert_eq!(plan.candidate_indices, vec![2]);
    }

    #[test]
    fn parse_gpu_report_roundtrip() {
        let raw = "\
MODE\tauto_integrated\n\
ACTIVE\t00000000:0000bbbb\t8086\ta7a0\t128\t0\tIntel(R) Iris(R) Xe Graphics\n\
ADAPTER\t00000000:0000bbbb\t8086\ta7a0\t128\t0\tIntel(R) Iris(R) Xe Graphics\n\
ADAPTER\t00000000:0000aaaa\t10de\t25a0\t8192\t0\tNVIDIA GeForce RTX 4060 Laptop GPU\n";
        let report = parse_gpu_report(raw);
        assert_eq!(report.mode, "auto_integrated");
        assert_eq!(report.discovered.len(), 2);
        let active = report.active.unwrap();
        assert_eq!(active.vendor_id, VENDOR_INTEL);
        assert_eq!(active.name, "Intel(R) Iris(R) Xe Graphics");
        assert!(!active.is_nvidia());
    }
}
