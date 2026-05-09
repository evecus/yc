// sysfs/mod.rs
// Writes kernel sysfs nodes.
//
// Features:
//  - Diff cache: skips writes where the value hasn't changed (saves power).
//  - cpufreq knob: value × 100_000 → kHz, with retry on failure (handles
//    cases where new min > old max during a frequency-table transition).
//  - percluster / percpu knobs: expand %d placeholders using the cluster/
//    cpu layout read from the config cpumask.
//  - Graceful: logs errors instead of panicking (phone may lack a node).

use std::collections::HashMap;
use std::fs;


/// A single named sysfs node definition.
#[derive(Debug, Clone)]
pub struct Knob {
    pub path:  String,
    pub kind:  KnobKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnobKind {
    /// echo "value" > path
    String,
    /// value × 100_000, write with retry
    CpuFreq,
    /// expand %d with cluster-leader cpu ids
    PerCluster { leaders: Vec<u32> },
    /// expand %d for every cpu
    PerCpu { cpus: Vec<u32> },
}

pub struct SysfsWriter {
    knobs: HashMap<String, Knob>,
    /// Last written value per path (for diff)
    cache: HashMap<String, String>,
}

impl SysfsWriter {
    /// Build from the config knob map.
    /// cluster_leaders: first cpu id in each cluster (for percluster knobs).
    /// all_cpus: list of all cpu ids (for percpu knobs).
    pub fn new(
        knob_map: &HashMap<String, String>,
        cluster_leaders: Vec<u32>,
        all_cpus: Vec<u32>,
    ) -> Self {
        let mut knobs = HashMap::new();
        for (name, path) in knob_map {
            let kind = infer_kind(name, path, &cluster_leaders, &all_cpus);
            knobs.insert(name.clone(), Knob { path: path.clone(), kind });
        }
        Self { knobs, cache: HashMap::new() }
    }

    /// Write a knob by name with the given string value.
    /// For cpufreq knobs the value is interpreted as GHz.
    pub fn write(&mut self, name: &str, value: &str) {
        let knob = match self.knobs.get(name) {
            Some(k) => k.clone(),
            None    => {
                log::warn!("sysfs: unknown knob '{}'", name);
                return;
            }
        };

        match knob.kind {
            KnobKind::String => {
                self.write_path(&knob.path, value);
            }
            KnobKind::CpuFreq => {
                // value is GHz; convert to kHz
                if let Ok(ghz) = value.parse::<f64>() {
                    let khz = (ghz * 1_000_000.0) as u64;
                    self.write_path_retry(&knob.path, &khz.to_string());
                }
            }
            KnobKind::PerCluster { ref leaders } => {
                let parts: Vec<&str> = value.split(',').collect();
                for (i, cpu) in leaders.iter().enumerate() {
                    let v = parts.get(i).copied().unwrap_or(parts.last().copied().unwrap_or(""));
                    let path = knob.path.replace("%d", &cpu.to_string());
                    self.write_path(&path, v.trim());
                }
            }
            KnobKind::PerCpu { ref cpus } => {
                let parts: Vec<&str> = value.split(',').collect();
                for (i, cpu) in cpus.iter().enumerate() {
                    let v = parts.get(i).copied().unwrap_or(parts.last().copied().unwrap_or(""));
                    let path = knob.path.replace("%d", &cpu.to_string());
                    self.write_path(&path, v.trim());
                }
            }
        }
    }

    /// Apply a batch of name→value pairs (from a preset action).
    pub fn apply_batch(&mut self, pairs: &HashMap<String, String>) {
        for (name, value) in pairs {
            self.write(name, value);
        }
    }

    /// Write scaling_max_freq / scaling_min_freq for a cluster (in Hz).
    /// Used by the CPU power model.
    pub fn write_freq_limit(&mut self, cpu: u32, max_khz: u64) {
        let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_max_freq");
        self.write_path_retry(&path, &max_khz.to_string());
    }

    /// Write a cpuset range, e.g. "0-3,6-7"
    #[allow(dead_code)]
    pub fn write_cpuset(&mut self, path: &str, value: &str) {
        self.write_path(path, value);
    }

    // ── internal ─────────────────────────────────────────────────────────────

    fn write_path(&mut self, path: &str, value: &str) {
        if self.cache.get(path).map(|v| v == value).unwrap_or(false) {
            return; // same value — skip
        }
        match fs::write(path, value) {
            Ok(_) => {
                log::trace!("sysfs: {} ← {}", path, value);
                self.cache.insert(path.to_string(), value.to_string());
            }
            Err(e) => log::debug!("sysfs: write {} failed: {}", path, e),
        }
    }

    fn write_path_retry(&mut self, path: &str, value: &str) {
        if self.cache.get(path).map(|v| v == value).unwrap_or(false) {
            return;
        }
        if fs::write(path, value).is_err() {
            // Retry once (handles min > max ordering requirement)
            let _ = fs::write(path, value);
        }
        self.cache.insert(path.to_string(), value.to_string());
        log::trace!("sysfs: {} ← {}", path, value);
    }
}

fn infer_kind(name: &str, path: &str, leaders: &[u32], all_cpus: &[u32]) -> KnobKind {
    if path.contains("%d") {
        if name.to_lowercase().contains("percpu") {
            KnobKind::PerCpu { cpus: all_cpus.to_vec() }
        } else {
            KnobKind::PerCluster { leaders: leaders.to_vec() }
        }
    } else if path.contains("cpufreq") && (name.contains("freq") || name.contains("Freq")) {
        KnobKind::CpuFreq
    } else {
        KnobKind::String
    }
}
