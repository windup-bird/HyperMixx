//! This process's CPU and resident memory, sampled once a second for the TUI header.
//!
//! `sysinfo` keeps the platform-specific `/proc`, Win32 and Mach details out of this crate, which
//! matters because the TUI is meant to stay portable to Windows. Refreshing a single PID is cheap;
//! the monitor still rate-limits itself so the 60Hz draw loop never pays for it.
//!
//! `sysinfo` reports process CPU the way `top` does: 100% per core. The header divides that by the
//! logical CPU count, so `cpu 12.5%` means 12.5% of the whole machine, not 12.5% of one core.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// One second is enough for a readable header and gives `sysinfo` the interval it needs to compute
/// a process CPU percentage.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

pub struct ProcessMonitor {
    system: System,
    pid: Pid,
    /// Logical CPUs available to this process, used to normalise `sysinfo`'s per-core percentage.
    cpus: f32,
    cpu_percent: Option<f32>,
    memory_bytes: Option<u64>,
    last: Option<Instant>,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let cpus = std::thread::available_parallelism().map_or(1.0, |count| count.get() as f32);
        let mut monitor = Self {
            system: System::new(),
            pid,
            cpus,
            cpu_percent: None,
            memory_bytes: None,
            last: None,
        };
        monitor.sample();
        monitor
    }

    /// Samples if a second has passed. Safe to call every UI tick.
    pub fn refresh(&mut self) {
        if self
            .last
            .is_some_and(|last| last.elapsed() < SAMPLE_EVERY)
        {
            return;
        }
        self.sample();
    }

    fn sample(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        if let Some(process) = self.system.process(self.pid) {
            self.cpu_percent = Some(normalise_cpu(process.cpu_usage(), self.cpus));
            self.memory_bytes = Some(process.memory());
        }
        self.last = Some(Instant::now());
    }

    /// Whole-machine CPU usage in percent (`0..=100`, not per-core).
    pub fn cpu_percent(&self) -> Option<f32> {
        self.cpu_percent
    }

    /// Resident set size in bytes.
    pub fn memory_bytes(&self) -> Option<u64> {
        self.memory_bytes
    }
}

/// `sysinfo` returns per-core percent (100 = one core). Divide by the logical CPU count so the
/// header reads as a share of the whole machine.
fn normalise_cpu(raw_percent: f32, cpus: f32) -> f32 {
    if !raw_percent.is_finite() {
        return 0.0;
    }
    (raw_percent / cpus.max(1.0)).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::normalise_cpu;

    #[test]
    fn process_cpu_is_normalised_by_the_machine_width() {
        // 200% top-style = two cores = 25% of an 8-core machine.
        assert_eq!(normalise_cpu(200.0, 8.0), 25.0);
        assert_eq!(normalise_cpu(50.0, 4.0), 12.5);
        assert_eq!(normalise_cpu(1000.0, 8.0), 100.0, "clamped to a machine share");
        assert_eq!(normalise_cpu(f32::NAN, 8.0), 0.0);
    }
}
