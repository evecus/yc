// cpu/mod.rs
// Userspace CPU frequency governor.
//
// The original uperf uses a per-cluster energy model to choose a
// frequency ceiling that bounds power draw to the configured limit
// while leaving headroom (margin) for burst loads.
//
// Model (from the original README):
//   power ≈ k × freq²  (simplified quadratic)
// The actual model is a piecewise linear fit anchored at typical_power @
// typical_freq, with different slopes for the efficiency / sweet-spot /
// linear / turbo regions.
//
// Here we implement:
//   1. ClusterGovernor: samples /proc/stat CPU utilisation, computes
//      the target frequency from the energy model, writes scaling_max_freq.
//   2. PowerModel: the per-cluster energy model arithmetic.

use std::fs;
use std::collections::HashMap;
use crate::config::{CpuModule, ClusterPowerModel, CpuInitials};
use crate::sysfs::SysfsWriter;

// ── Power model ───────────────────────────────────────────────────────────────

pub struct PowerModel {
    clusters: Vec<ClusterModel>,
}

struct ClusterModel {
    cfg:           ClusterPowerModel,
    /// cpu ids that belong to this cluster
    cpus:          Vec<u32>,
    /// frequencies available on this cluster (kHz), sorted ascending
    available_khz: Vec<u64>,
}

impl PowerModel {
    pub fn new(module: &CpuModule, cpumask: &HashMap<String, Vec<u32>>) -> Self {
        // Match clusters by index (c0, c1, c2 …)
        let cluster_keys = {
            let mut keys: Vec<String> = cpumask.keys()
                .filter(|k| k.starts_with('c') && k[1..].parse::<u32>().is_ok())
                .cloned()
                .collect();
            keys.sort_by_key(|k| k[1..].parse::<u32>().unwrap_or(99));
            keys
        };

        let clusters = module.power_model.iter().enumerate().map(|(i, cfg)| {
            let cpus = cluster_keys.get(i)
                .and_then(|k| cpumask.get(k))
                .cloned()
                .unwrap_or_default();
            let available_khz = cpus.first()
                .map(|&cpu| read_available_freqs(cpu))
                .unwrap_or_default();
            ClusterModel { cfg: cfg.clone(), cpus, available_khz }
        }).collect();

        Self { clusters }
    }

    /// Compute per-cluster max frequency limits (kHz) for a given
    /// total power budget (watts) and utilisation headroom margin.
    pub fn compute_limits(
        &self,
        max_power_per_cluster: f64,
        margin: f64,
        utilisation: &[f64],  // per-cluster avg utilisation [0,1]
    ) -> Vec<(Vec<u32>, u64)> {
        self.clusters.iter().enumerate().map(|(i, cluster)| {
            let util  = utilisation.get(i).copied().unwrap_or(1.0);
            let power = max_power_per_cluster * (util + margin).min(1.5);
            let freq  = cluster.power_to_freq_khz(power);
            (cluster.cpus.clone(), freq)
        }).collect()
    }

    pub fn cluster_cpus(&self) -> Vec<Vec<u32>> {
        self.clusters.iter().map(|c| c.cpus.clone()).collect()
    }
}

impl ClusterModel {
    /// Given a target power budget, return the highest frequency (kHz)
    /// that stays within it.
    fn power_to_freq_khz(&self, target_power_w: f64) -> u64 {
        let cfg = &self.cfg;
        // Typical operating point
        let tp = cfg.typical_power;
        let tf = cfg.typical_freq; // GHz

        // Simple quadratic model: P ∝ f²
        // f_target = tf × sqrt(target_power / typical_power)
        let ratio = if tp > 0.0 { target_power_w / tp } else { 1.0 };
        let f_ghz = tf * ratio.sqrt();
        let f_khz = (f_ghz * 1_000_000.0) as u64;

        // Snap to the nearest available frequency (round down)
        self.snap_freq(f_khz)
    }

    fn snap_freq(&self, target_khz: u64) -> u64 {
        if self.available_khz.is_empty() {
            return target_khz;
        }
        // highest available freq ≤ target
        match self.available_khz.partition_point(|&f| f <= target_khz) {
            0 => *self.available_khz.first().unwrap(),
            n => self.available_khz[n - 1],
        }
    }
}

/// Read /sys/.../scaling_available_frequencies for a given cpu.
fn read_available_freqs(cpu: u32) -> Vec<u64> {
    let path = format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_available_frequencies"
    );
    let raw = match fs::read_to_string(&path) {
        Ok(s)  => s,
        Err(_) => return vec![],
    };
    let mut freqs: Vec<u64> = raw.split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    freqs.sort_unstable();
    freqs
}

// ── CPU utilisation sampler ───────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CpuStat {
    idle:  u64,
    total: u64,
}

pub struct Governor {
    model:       PowerModel,
    prev_stats:  Vec<CpuStat>,
    // tunables (set per-hint via initials/presets)
    pub max_power:  f64,   // watts per cluster
    pub margin:     f64,
    pub limit_eff:  bool,
}

impl Governor {
    pub fn new(model: PowerModel, init: &CpuInitials) -> Self {
        Self {
            model,
            prev_stats: Vec::new(),
            max_power:  init.fast_limit_power,
            margin:     init.margin,
            limit_eff:  init.limit_efficiency,
        }
    }

    /// Sample CPU util and write frequency limits.
    pub fn apply(&mut self, writer: &mut SysfsWriter) {
        let stats = read_proc_stat();
        let utils = self.compute_utils(&stats);
        self.prev_stats = stats;

        let limits = self.model.compute_limits(self.max_power, self.margin, &utils);
        for (cpus, khz) in limits {
            for cpu in cpus {
                writer.write_freq_limit(cpu, khz);
            }
        }
    }

    fn compute_utils(&self, current: &[CpuStat]) -> Vec<f64> {
        if self.prev_stats.is_empty() || current.len() != self.prev_stats.len() {
            return vec![0.5; self.model.cluster_cpus().len()];
        }
        // Compute per-cluster average utilisation
        let cluster_cpus = self.model.cluster_cpus();
        cluster_cpus.iter().map(|cpus| {
            let sum: f64 = cpus.iter().map(|&cpu| {
                let i = cpu as usize;
                if i >= current.len() { return 0.5; }
                let d_total = current[i].total.saturating_sub(self.prev_stats[i].total);
                let d_idle  = current[i].idle.saturating_sub(self.prev_stats[i].idle);
                if d_total == 0 { 0.0 }
                else { 1.0 - d_idle as f64 / d_total as f64 }
            }).sum::<f64>();
            (sum / cpus.len() as f64).clamp(0.0, 1.0)
        }).collect()
    }
}

/// Read /proc/stat and return per-cpu (idle, total) pairs.
fn read_proc_stat() -> Vec<CpuStat> {
    let raw = match fs::read_to_string("/proc/stat") {
        Ok(s)  => s,
        Err(_) => return vec![],
    };
    let mut stats = Vec::new();
    for line in raw.lines() {
        if !line.starts_with("cpu") { continue; }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 { continue; }
        // "cpu0 user nice system idle iowait irq softirq …"
        if parts[0] == "cpu" { continue; } // aggregate line
        let nums: Vec<u64> = parts[1..].iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        if nums.len() < 4 { continue; }
        let total: u64 = nums.iter().sum();
        let idle = nums[3]; // idle + iowait
        let iowait = nums.get(4).copied().unwrap_or(0);
        stats.push(CpuStat { idle: idle + iowait, total });
    }
    stats
}
