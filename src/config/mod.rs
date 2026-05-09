// config/mod.rs
// Deserialises the uperf JSON config.  Field names mirror the original
// camelCase schema so existing config files work without modification.

use std::collections::HashMap;
use anyhow::{Context, Result};
use serde::Deserialize;

// ── Top-level ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub meta:     Meta,
    pub modules:  Modules,
    pub initials: Initials,
    pub presets:  HashMap<String, Preset>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read config: {path}"))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("Cannot parse config: {path}"))
    }
}

// ── Meta ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct Meta {
    pub name:   String,
    pub author: String,
}

// ── Modules ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Modules {
    pub switcher:   SwitcherModule,
    pub log:        LogModule,
    pub input:      InputModule,
    pub sfanalysis: SfAnalysisModule,
    pub cpu:        CpuModule,
    pub sysfs:      SysfsModule,
    pub sched:      SchedModule,
}

// switcher
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SwitcherModule {
    pub switch_inode:  String,
    pub perapp:        String,
    pub hint_duration: HintDuration,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HintDuration {
    pub idle:    f64,
    pub touch:   f64,
    pub trigger: f64,
    pub gesture: f64,
    pub switch:  f64,
    pub junk:    f64,
}

// log
#[derive(Debug, Deserialize, Clone)]
pub struct LogModule {
    pub level: String,
}

// input
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InputModule {
    pub enable:            bool,
    pub swipe_thd:         f64,
    pub gesture_thd_x:     f64,
    pub gesture_thd_y:     f64,
    pub gesture_delay_time: f64,
    pub hold_enter_time:   f64,
}

// sfanalysis (we keep the field but always disable it)
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SfAnalysisModule {
    pub enable:                bool,
    pub render_idle_slack_time: f64,
}

// cpu
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CpuModule {
    pub enable:      bool,
    pub power_model: Vec<ClusterPowerModel>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClusterPowerModel {
    /// Single-core performance relative to Cortex-A53 @ 1 GHz = 100
    pub efficiency:    u32,
    /// Number of cores in this cluster
    pub nr:            u32,
    /// Typical single-core power draw (W)
    pub typical_power: f64,
    /// Frequency at which typical_power was measured (GHz)
    pub typical_freq:  f64,
    /// "Sweet-spot" boundary frequency (GHz)
    pub sweet_freq:    f64,
    /// Linear-range boundary frequency (GHz)
    pub plain_freq:    f64,
    /// Lowest-power frequency (GHz)
    pub free_freq:     f64,
}

// sysfs
#[derive(Debug, Deserialize, Clone)]
pub struct SysfsModule {
    pub enable: bool,
    pub knob:   HashMap<String, String>,
}

// sched
#[derive(Debug, Deserialize, Clone)]
pub struct SchedModule {
    pub enable:   bool,
    pub cpumask:  HashMap<String, Vec<u32>>,
    pub affinity: HashMap<String, SceneAffinity>,
    pub prio:     HashMap<String, ScenePrio>,
    pub rules:    Vec<ProcessRule>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SceneAffinity {
    pub bg:    String,
    pub fg:    String,
    pub idle:  String,
    pub touch: String,
    pub boost: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScenePrio {
    pub bg:    i32,
    pub fg:    i32,
    pub idle:  i32,
    pub touch: i32,
    pub boost: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProcessRule {
    pub name:   String,
    pub regex:  String,
    pub pinned: bool,
    pub rules:  Vec<ThreadRule>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ThreadRule {
    /// Thread-name regex key
    pub k:  String,
    /// Affinity class name
    pub ac: String,
    /// Priority class name
    pub pc: String,
}

// ── Initials ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct Initials {
    pub cpu:   CpuInitials,
    pub sysfs: HashMap<String, String>,
    pub sched: SchedInitials,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CpuInitials {
    pub base_sample_time:        f64,
    pub base_slack_time:         f64,
    pub latency_time:            f64,
    pub slow_limit_power:        f64,
    pub fast_limit_power:        f64,
    pub fast_limit_capacity:     f64,
    pub fast_limit_recover_scale: f64,
    pub predict_thd:             f64,
    pub margin:                  f64,
    pub burst:                   f64,
    pub guide_cap:               bool,
    pub limit_efficiency:        bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SchedInitials {
    pub scene: String,
}

// ── Presets ───────────────────────────────────────────────────────────────────

/// A preset maps hint names → dynamic parameter overrides.
/// The special key "*" applies to all hints.
pub type Preset = HashMap<String, HashMap<String, serde_json::Value>>;
