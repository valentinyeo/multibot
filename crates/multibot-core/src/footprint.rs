use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Debug, Clone, Copy, Default)]
pub struct FootprintSnapshot {
    pub memory_mb: f64,
    pub cpu_percent: f32,
}

/// Samples ONLY the current process (avoids scanning every process on Windows)
/// so status-card telemetry has negligible CPU overhead.
pub fn sample_self_footprint(system: &mut System) -> FootprintSnapshot {
    let pid = Pid::from_u32(std::process::id());
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    if let Some(proc_info) = system.process(pid) {
        FootprintSnapshot {
            memory_mb: (proc_info.memory() as f64) / (1024.0 * 1024.0),
            cpu_percent: proc_info.cpu_usage(),
        }
    } else {
        FootprintSnapshot::default()
    }
}
