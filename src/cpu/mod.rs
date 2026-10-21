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
//   1. ClusterGovernor: samples top-app cgroup utilisation (cgroup v1
//      cpuacct per-cpu, global /proc/stat fallback), computes the target
//      frequency from the piecewise energy model, writes scaling_max_freq.
//   2. PowerModel: the per-cluster energy model arithmetic.
//
// Optimizations vs the first rewrite:
//   • Utilisation is sampled from the foreground app's cgroup instead of
//     the global /proc/stat, so background load no longer skews decisions.
//   • Write hysteresis: raise immediately, lower only past a 5% drop,
//     and never write the same cluster twice within 100 ms — avoids
//     frequency fighting with the in-kernel governor (schedutil/WALT).
//   • The piecewise model now uses the free/sweet/plain fields from the
//     config (they were dead fields in the first rewrite).

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};
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
    ///
    /// Piecewise model: base quadratic P ∝ f² anchored at
    /// (typical_freq, typical_power), scaled by a region factor that
    /// reflects the efficiency / sweet-spot / linear / turbo regions
    /// declared in the config (free_freq / sweet_freq / plain_freq).
    fn power_to_freq_khz(&self, target_power_w: f64) -> u64 {
        let cfg = &self.cfg;
        let tp = cfg.typical_power;
        let tf = cfg.typical_freq;

        let max_ghz = if !self.available_khz.is_empty() {
            *self.available_khz.last().unwrap() as f64 / 1_000_000.0
        } else {
            (cfg.plain_freq * 1.3).max(1.0)
        };

        if tp <= 0.0 || tf <= 0.0 {
            // No usable model — pin to max
            return self.snap_freq((max_ghz * 1_000_000.0) as u64);
        }

        // Region factor: efficiency core region is cheap, turbo is expensive
        let region_factor = |f: f64| -> f64 {
            if f < cfg.free_freq       { 0.65 }
            else if f < cfg.sweet_freq { 0.80 }
            else if f < cfg.plain_freq { 1.00 }
            else                       { 1.30 }
        };
        let power_at = |f: f64| -> f64 {
            tp * (f / tf) * (f / tf) * region_factor(f)
        };

        let lo = (cfg.free_freq * 0.5).max(0.05);
        if power_at(lo) > target_power_w {
            // Even the lowest frequency exceeds the budget
            return self.snap_freq((lo * 1_000_000.0) as u64);
        }
        // Binary search: highest f with power_at(f) <= target
        let (mut lo, mut hi) = (lo, max_ghz);
        for _ in 0..48 {
            let mid = (lo + hi) / 2.0;
            if power_at(mid) <= target_power_w { lo = mid; } else { hi = mid; }
        }
        self.snap_freq((lo * 1_000_000.0) as u64)
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

/// Utilisation source preference:
///   1. cgroup v1 cpuacct of top-app (per-cpu usage, foreground only)
///   2. global /proc/stat (fallback; forced to zero when screen off)
pub struct UtilSampler {
    v1_path:   Option<PathBuf>,
    prev_cg:   Vec<u64>,        // v1 per-cpu cumulative ns
    prev_proc: Vec<CpuStat>,    // global fallback
    last_t:    Instant,
    inited:    bool,
}

impl UtilSampler {
    pub fn new() -> Self {
        let v1 = PathBuf::from("/sys/fs/cgroup/cpuacct/top-app/cpuacct.usage_percpu");
        let v1_path = if v1.exists() { Some(v1) } else { None };
        match &v1_path {
            Some(p) => log::info!("util: sampling top-app cgroup v1 ({})", p.display()),
            None    => log::info!("util: cgroup v1 cpuacct unavailable, falling back to /proc/stat"),
        }
        Self {
            v1_path,
            prev_cg: Vec::new(),
            prev_proc: Vec::new(),
            last_t: Instant::now(),
            inited: false,
        }
    }

    /// Per-cluster average utilisation [0,1].
    /// `front_empty` = no foreground app (screen off) → all zero.
    pub fn sample(&mut self, clusters: &[Vec<u32>], front_empty: bool) -> Vec<f64> {
        let n = clusters.len();
        let now = Instant::now();
        let dt_ns = now.saturating_duration_since(self.last_t).as_nanos() as f64;
        self.last_t = now;

        if front_empty {
            // Reset baseline so the next sample after screen-on is clean
            self.inited = false;
            return vec![0.0; n];
        }

        // ── cgroup v1: foreground per-cpu usage ──
        if let Some(path) = &self.v1_path {
            if let Ok(raw) = fs::read_to_string(path) {
                let usage: Vec<u64> = raw
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if usage.len() > 1 {
                    if !self.inited || usage.len() != self.prev_cg.len() {
                        self.prev_cg = usage;
                        self.inited = true;
                        return vec![0.5; n];
                    }
                    let delta: Vec<u64> = usage
                        .iter()
                        .zip(self.prev_cg.iter())
                        .map(|(a, b)| a.saturating_sub(*b))
                        .collect();
                    self.prev_cg = usage;
                    if dt_ns <= 0.0 {
                        return vec![0.5; n];
                    }
                    return clusters
                        .iter()
                        .map(|cpus| {
                            let sum: f64 = cpus
                                .iter()
                                .filter_map(|&c| delta.get(c as usize))
                                .map(|&u| u as f64 / dt_ns)
                                .sum();
                            (sum / cpus.len() as f64).clamp(0.0, 1.0)
                        })
                        .collect();
                }
            }
        }

        // ── fallback: global /proc/stat ──
        let stats = read_proc_stat();
        if self.prev_proc.is_empty() || stats.len() != self.prev_proc.len() {
            self.prev_proc = stats;
            return vec![0.5; n];
        }
        let utils = clusters
            .iter()
            .map(|cpus| {
                let sum: f64 = cpus
                    .iter()
                    .map(|&cpu| {
                        let i = cpu as usize;
                        if i >= stats.len() { return 0.5; }
                        let d_total = stats[i].total.saturating_sub(self.prev_proc[i].total);
                        let d_idle  = stats[i].idle.saturating_sub(self.prev_proc[i].idle);
                        if d_total == 0 { 0.0 }
                        else { 1.0 - d_idle as f64 / d_total as f64 }
                    })
                    .sum::<f64>();
                (sum / cpus.len() as f64).clamp(0.0, 1.0)
            })
            .collect();
        self.prev_proc = stats;
        utils
    }
}

// ── Governor ──────────────────────────────────────────────────────────────────

pub struct Governor {
    model:       PowerModel,
    sampler:     UtilSampler,
    // tunables (set per-hint via initials/presets)
    pub max_power:  f64,   // watts per cluster
    pub margin:     f64,
    pub limit_eff:  bool,
    // write hysteresis state (per cluster)
    cur_limits:  Vec<u64>,
    last_write:  Vec<Option<Instant>>,
}

/// Minimum interval between two writes to the same cluster (ms).
const MIN_WRITE_INTERVAL_MS: u64 = 100;
/// Lower the limit only past this relative drop (denominator, 20 → 5 %).
const DOWN_HYSTERESIS: u64 = 20;

impl Governor {
    pub fn new(model: PowerModel, init: &CpuInitials) -> Self {
        let n = model.cluster_cpus().len();
        Self {
            model,
            sampler: UtilSampler::new(),
            max_power:  init.fast_limit_power,
            margin:     init.margin,
            limit_eff:  init.limit_efficiency,
            cur_limits:  vec![0; n],
            last_write:  vec![None; n],
        }
    }

    /// Sample CPU util and write frequency limits (with hysteresis).
    pub fn apply(&mut self, writer: &mut SysfsWriter, front_empty: bool) {
        let clusters = self.model.cluster_cpus();
        let utils = self.sampler.sample(&clusters, front_empty);

        let mut limits = self.model.compute_limits(self.max_power, self.margin, &utils);
        if self.limit_eff {
            // Efficiency limiting: shave 15% off the ceiling
            for (_, khz) in limits.iter_mut() {
                *khz = *khz - *khz * 15 / 100;
            }
        }

        let now = Instant::now();
        for (i, (cpus, khz)) in limits.iter().enumerate() {
            let cur = self.cur_limits.get(i).copied().unwrap_or(0);
            let up   = *khz > cur;
            let down = cur > 0 && *khz < cur - cur / DOWN_HYSTERESIS;
            let interval_ok = self.last_write.get(i)
                .and_then(|t| *t)
                .map(|t| t.elapsed() >= Duration::from_millis(MIN_WRITE_INTERVAL_MS))
                .unwrap_or(true);

            if (up || down) && interval_ok {
                for cpu in cpus {
                    writer.write_freq_limit(*cpu, *khz);
                }
                if i < self.cur_limits.len() { self.cur_limits[i] = *khz; }
                if i < self.last_write.len()  { self.last_write[i] = Some(now); }
            }
        }
    }

    /// Force the next apply() to write every cluster (used after mode switch).
    pub fn force_write_next(&mut self) {
        for v in self.cur_limits.iter_mut() { *v = 0; }
        for t in self.last_write.iter_mut()  { *t = None; }
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
