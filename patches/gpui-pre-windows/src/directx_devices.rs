use anyhow::{Context, Result};
use gpui_util::ResultExt;
use itertools::Itertools;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_DEBUG,
            DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_MINIMUM_POWER, IDXGIAdapter1,
            IDXGIFactory6,
        },
    },
};

const VENDOR_NVIDIA: u32 = 0x10DE;
const ENV_GPU_MODE: &str = "MULTIBOT_GPU_MODE";
const ENV_GPU_LUID: &str = "MULTIBOT_GPU_LUID";
const ENV_GPU_REPORT_PATH: &str = "MULTIBOT_GPU_REPORT_PATH";

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let (adapter, device, device_context, feature_level) =
            get_adapter(&dxgi_factory, debug_layer_available).context("Getting DXGI adapter")?;
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
        })
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

struct DiscoveredAdapter {
    adapter: IDXGIAdapter1,
    luid: String,
    vendor_id: u32,
    device_id: u32,
    dedicated_vram_mb: u64,
    is_software: bool,
    name: String,
}

impl DiscoveredAdapter {
    fn is_nvidia(&self) -> bool {
        self.vendor_id == VENDOR_NVIDIA
    }

    fn format_fields(&self) -> String {
        let clean_name = self.name.replace(['\t', '\r', '\n'], " ");
        format!(
            "{}\t{:04x}\t{:04x}\t{}\t{}\t{}",
            self.luid,
            self.vendor_id,
            self.device_id,
            self.dedicated_vram_mb,
            if self.is_software { 1 } else { 0 },
            clean_name
        )
    }
}

fn write_gpu_report(mode: &str, active: Option<&DiscoveredAdapter>, discovered: &[DiscoveredAdapter]) {
    let Ok(path) = std::env::var(ENV_GPU_REPORT_PATH) else {
        return;
    };
    if path.trim().is_empty() {
        return;
    }
    let mut out = String::new();
    out.push_str(&format!("MODE\t{}\n", mode));
    if let Some(chosen) = active {
        out.push_str(&format!("ACTIVE\t{}\n", chosen.format_fields()));
    }
    for item in discovered {
        out.push_str(&format!("ADAPTER\t{}\n", item.format_fields()));
    }
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, out);
}

fn enumerate_low_power_adapters(dxgi_factory: &IDXGIFactory6) -> Vec<DiscoveredAdapter> {
    let mut discovered = Vec::new();
    for adapter_index in 0.. {
        let adapter: IDXGIAdapter1 = match unsafe {
            dxgi_factory.EnumAdapterByGpuPreference(
                adapter_index,
                DXGI_GPU_PREFERENCE_MINIMUM_POWER,
            )
        } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            let luid = format!(
                "{:08x}:{:08x}",
                desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
            );
            let is_software = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
            let dedicated_vram_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
            discovered.push(DiscoveredAdapter {
                adapter,
                luid,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                dedicated_vram_mb,
                is_software,
                name,
            });
        }
    }
    discovered
}

#[inline]
fn get_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    let discovered = enumerate_low_power_adapters(dxgi_factory);
    let mode = std::env::var(ENV_GPU_MODE).unwrap_or_else(|_| "auto_integrated".to_string());
    let requested_luid = std::env::var(ENV_GPU_LUID).ok();

    let mut candidates: Vec<usize> = Vec::new();
    match mode.as_str() {
        "specific_luid" => {
            if let Some(ref target) = requested_luid {
                if let Some(idx) = discovered
                    .iter()
                    .position(|a| a.luid.eq_ignore_ascii_case(target))
                {
                    candidates.push(idx);
                }
            }
            for (i, a) in discovered.iter().enumerate() {
                if !candidates.contains(&i) && !a.is_software && !a.is_nvidia() {
                    candidates.push(i);
                }
            }
            for (i, a) in discovered.iter().enumerate() {
                if !candidates.contains(&i) && a.is_software {
                    candidates.push(i);
                }
            }
        }
        "software_only" => {
            for (i, a) in discovered.iter().enumerate() {
                if a.is_software {
                    candidates.push(i);
                }
            }
        }
        _ => {
            // Default auto_integrated: prefer non-NVIDIA hardware, fall back to WARP software,
            // and NEVER touch NVIDIA (0x10DE).
            for (i, a) in discovered.iter().enumerate() {
                if !a.is_software && !a.is_nvidia() {
                    candidates.push(i);
                }
            }
            for (i, a) in discovered.iter().enumerate() {
                if a.is_software {
                    candidates.push(i);
                }
            }
        }
    }

    for idx in candidates {
        let item = &discovered[idx];
        log::info!(
            "Attempting GPU (mode={}): {} [vendor=0x{:04x}, luid={}, software={}]",
            mode,
            item.name,
            item.vendor_id,
            item.luid,
            item.is_software
        );
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            &item.adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            log::info!("Selected GPU: {} [luid={}]", item.name, item.luid);
            write_gpu_report(&mode, Some(item), &discovered);
            return Ok((
                item.adapter.clone(),
                device,
                context.unwrap(),
                feature_level,
            ));
        }
    }

    write_gpu_report(&mode, None, &discovered);
    Err(anyhow::anyhow!(
        "No compatible non-NVIDIA Direct3D 11 adapter found (mode={})",
        mode
    ))
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )?;
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}
