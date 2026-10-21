// platform/mod.rs
// 运行时平台自动探测（auto 模式）
//
// 目的：对未收录的新处理器，无需手写 JSON 也能获得一份可用配置。
// 探测内容：
//   1. CPU cluster 拓扑   —— /sys/devices/system/cpu/cpu*/cpufreq/related_cpus 分组
//   2. 频率表             —— cpuinfo_max_freq + scaling_available_frequencies
//   3. GPU 接口与频率      —— kgsl / mali devfreq 的 available_frequencies
//   4. 功耗模型（估算）    —— 按峰值频率启发式估算 typicalPower/typicalFreq 等
//
// 平台无关部分（线程规则 / affinity / prio / presets 骨架）以 config/sd8gen3.json
// 为默认模板（编译期 include_str! 嵌入），探测结果覆盖其中的平台相关字段。
//
// 触发方式：
//   - platform.json 缺失 / 解析失败
//   - platform.json 的 meta.name == "auto"
//   - service.sh 在未知平台时写入 {"meta":{"name":"auto"}} 标记文件

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::config::{ClusterPowerModel, Config};

/// 默认模板：骁龙 8 Gen3 配置（平台无关字段的来源）
const DEFAULT_TEMPLATE: &str = include_str!("../../config/sd8gen3.json");

// ── 探测结果 ──────────────────────────────────────────────────────────────────

pub struct ClusterInfo {
    /// 该 cluster 的 CPU 编号（升序）
    pub cpus: Vec<u32>,
    /// cluster 代表 CPU（最小编号，cpufreq 节点通常以它为准）
    pub leader: u32,
    /// 最高频率（kHz）
    pub max_khz: u64,
    /// 可用 OPP 表（kHz，升序；可能为空）
    pub available_khz: Vec<u64>,
}

// ── CPU 拓扑探测 ──────────────────────────────────────────────────────────────

/// 探测 CPU cluster 拓扑。优先 related_cpus；无 cpufreq 时按峰值频率聚类。
pub fn detect_clusters() -> Vec<ClusterInfo> {
    let cpus = list_cpus();
    if cpus.is_empty() {
        return Vec::new();
    }

    // 1) related_cpus 分组（同一 cpufreq policy 的核列相同集合）
    let mut groups: Vec<Vec<u32>> = Vec::new();
    let mut has_cpufreq = false;
    for &cpu in &cpus {
        if let Some(rel) = read_related_cpus(cpu) {
            has_cpufreq = true;
            let mut g = rel;
            g.sort_unstable();
            if !groups.iter().any(|x| *x == g) {
                groups.push(g);
            }
        }
    }

    // 2) fallback：按峰值频率聚类（相差 <8% 视为同 cluster）
    if !has_cpufreq {
        let mut by_freq: Vec<(u64, Vec<u32>)> = Vec::new();
        for &cpu in &cpus {
            let f = read_max_freq(cpu);
            match by_freq.iter_mut().find(|(bf, _)| {
                let hi = (*bf).max(f) as f64;
                let lo = (*bf).min(f) as f64;
                hi > 0.0 && (hi - lo) / hi < 0.08
            }) {
                Some((bf, ref mut cpus_g)) => {
                    *bf = (*bf).max(f);
                    cpus_g.push(cpu);
                }
                None => by_freq.push((f, vec![cpu])),
            }
        }
        by_freq.sort_by_key(|(f, _)| *f);
        groups = by_freq.into_iter().map(|(_, g)| g).collect();
    }

    groups.sort_by_key(|g| g[0]);
    groups
        .into_iter()
        .map(|cpus| {
            let leader = cpus[0];
            ClusterInfo {
                cpus,
                leader,
                max_khz: read_max_freq(leader),
                available_khz: read_available_freqs(leader),
            }
        })
        .collect()
}

fn list_cpus() -> Vec<u32> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/devices/system/cpu") {
        for e in rd.flatten() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if let Some(rest) = s.strip_prefix("cpu") {
                if let Ok(n) = rest.parse::<u32>() {
                    // 排除 cpufreq / cpuidle 等子目录
                    if !rest.contains("cpufreq") && !rest.contains("cpuidle") {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

fn read_related_cpus(cpu: u32) -> Option<Vec<u32>> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/related_cpus");
    let raw = fs::read_to_string(&path).ok()?;
    let v: Vec<u32> = raw
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if v.is_empty() { None } else { Some(v) }
}

fn read_max_freq(cpu: u32) -> u64 {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn read_available_freqs(cpu: u32) -> Vec<u64> {
    let path = format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_available_frequencies"
    );
    let mut freqs: Vec<u64> = fs::read_to_string(&path)
        .ok()
        .map(|s| {
            s.split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    freqs.sort_unstable();
    freqs
}

// ── GPU 探测 ──────────────────────────────────────────────────────────────────

/// 探测 GPU devfreq 频率，返回 (min_khz, mid_khz, max_khz)。
/// Adreno（kgsl）优先，其次 Mali（devfreq 名字含 mali/gpu/mfg）。
pub fn detect_gpu_freqs() -> Option<(u64, u64, u64)> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Adreno kgsl（必须先于通用 devfreq 扫描，避免误匹配 gpubw）
    for base in [
        "/sys/class/kgsl/kgsl-3d0/devfreq",
        "/sys/devices/platform/soc/3d00000.qcom,kgsl-3d0/devfreq/3d00000.qcom,kgsl-3d0",
    ] {
        let p = PathBuf::from(base);
        if p.join("max_freq").exists() && p.join("min_freq").exists() {
            candidates.push(p);
        }
    }

    // Mali / 通用 devfreq
    if let Ok(rd) = fs::read_dir("/sys/class/devfreq") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.contains("bw") || name.contains("ddr") {
                continue;
            }
            if name.contains("mali") || name.contains("gpu") || name.contains("mfg") {
                candidates.push(e.path());
            }
        }
    }

    for dir in candidates {
        if let Some(freqs) = read_devfreq_freqs(&dir) {
            if freqs.len() >= 2 {
                let min = freqs[0];
                let max = *freqs.last().unwrap();
                let mid = freqs[freqs.len() / 2];
                return Some((min, mid, max));
            }
        }
    }
    None
}

/// 读 devfreq 的 available_frequencies（单位 Hz），返回升序 kHz 表。
fn read_devfreq_freqs(dir: &PathBuf) -> Option<Vec<u64>> {
    let path = dir.join("available_frequencies");
    let raw = fs::read_to_string(&path).ok()?;
    let mut freqs: Vec<u64> = raw
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .map(|hz| hz / 1000) // Hz → kHz
        .collect();
    freqs.sort_unstable();
    if freqs.is_empty() { None } else { Some(freqs) }
}

// ── 前台 cgroup 辅助 ──────────────────────────────────────────────────────────

/// top-app cgroup 是否为空（息屏判定 fallback；读不到时返回 false 不误判）。
pub fn top_app_empty() -> bool {
    for p in [
        "/dev/cpuset/top-app/cgroup.procs",
        "/sys/fs/cgroup/top-app/cgroup.procs",
    ] {
        if let Ok(s) = fs::read_to_string(p) {
            return s.trim().is_empty();
        }
    }
    false
}

/// 当前前台应用包名（取 top-app cgroup 第一个进程的 cmdline）。
pub fn top_app_package() -> Option<String> {
    for p in [
        "/dev/cpuset/top-app/cgroup.procs",
        "/sys/fs/cgroup/top-app/cgroup.procs",
    ] {
        if let Ok(s) = fs::read_to_string(p) {
            let line = s.lines().find(|l| !l.trim().is_empty())?;
            let pid: u32 = line.trim().parse().ok()?;
            let cmd = fs::read_to_string(format!("/proc/{pid}/cmdline"))
                .unwrap_or_default();
            let pkg = cmd.split('\0').next().unwrap_or("").trim();
            if !pkg.is_empty() {
                return Some(pkg.to_string());
            }
            return None;
        }
    }
    None
}

// ── auto 配置构建 ─────────────────────────────────────────────────────────────

/// 构建 auto 配置：默认模板 + 运行时探测覆盖。
pub fn build_auto_config() -> Config {
    let clusters = detect_clusters();
    log::info!(
        "platform: auto-detected {} cluster(s): {:?}",
        clusters.len(),
        clusters
            .iter()
            .map(|c| format!("cpu{}-{} @{}MHz", c.cpus.first().copied().unwrap_or(0),
                             c.cpus.last().copied().unwrap_or(0), c.max_khz / 1000))
            .collect::<Vec<_>>()
    );

    let mut cfg: Value = serde_json::from_str(DEFAULT_TEMPLATE)
        .expect("built-in default template must parse");

    if clusters.is_empty() {
        log::warn!("platform: no cpufreq info — using template as-is");
        return serde_json::from_value(cfg).expect("template config must be valid");
    }

    cfg["meta"]["name"] = json!("auto-detected");

    // ── 1. cpu.powerModel（启发式估算） ──────────────────────────────────────
    let models: Vec<Value> = clusters
        .iter()
        .map(|c| {
            let max_ghz = c.max_khz as f64 / 1_000_000.0;
            let m = ClusterPowerModel {
                efficiency:    (100.0 + max_ghz * 110.0) as u32,
                nr:            c.cpus.len() as u32,
                typical_power: (max_ghz * 0.75 + 0.15).max(0.10),
                typical_freq:  (max_ghz * 0.45).max(0.50),
                sweet_freq:    max_ghz * 0.65,
                plain_freq:    max_ghz * 0.85,
                free_freq:     (max_ghz * 0.30).max(0.40),
            };
            json!({
                "_comment": format!("auto-estimated cluster (cpu{}-{})", c.cpus[0], c.cpus[c.cpus.len()-1]),
                "efficiency": m.efficiency,
                "nr": m.nr,
                "typicalPower": m.typical_power,
                "typicalFreq": m.typical_freq,
                "sweetFreq": m.sweet_freq,
                "plainFreq": m.plain_freq,
                "freeFreq": m.free_freq,
            })
        })
        .collect();
    cfg["modules"]["cpu"]["powerModel"] = Value::Array(models);

    // ── 2. sysfs.knob ────────────────────────────────────────────────────────
    let mut knob = Map::new();
    for (k, v) in [
        ("cpusetTa",    "/dev/cpuset/top-app/cpus"),
        ("cpusetFg",    "/dev/cpuset/foreground/cpus"),
        ("cpusetBg",    "/dev/cpuset/background/cpus"),
        ("cpusetSysBg", "/dev/cpuset/system-background/cpus"),
        ("cpusetRe",    "/dev/cpuset/restricted/cpus"),
    ] {
        knob.insert(k.to_string(), json!(v));
    }
    for (i, c) in clusters.iter().enumerate() {
        knob.insert(
            format!("c{i}MaxFreq"),
            json!(format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_max_freq", c.leader)),
        );
        knob.insert(
            format!("c{i}MinFreq"),
            json!(format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_min_freq", c.leader)),
        );
    }
    knob.insert("cpuSchedBoost".to_string(), json!("/proc/sys/walt/sched_boost"));
    cfg["modules"]["sysfs"]["knob"] = Value::Object(knob);

    // ── 3. sched.cpumask ─────────────────────────────────────────────────────
    let all: Vec<u32> = clusters.iter().flat_map(|c| c.cpus.iter().copied()).collect();
    let mid: Vec<u32> = if clusters.len() > 1 {
        clusters[1..].iter().flat_map(|c| c.cpus.iter().copied()).collect()
    } else {
        all.clone()
    };
    let mut mask = Map::new();
    mask.insert("all".to_string(), json!(all));
    for (i, c) in clusters.iter().enumerate() {
        mask.insert(format!("c{i}"), json!(c.cpus));
    }
    mask.insert("mid".to_string(), json!(mid));
    cfg["modules"]["sched"]["cpumask"] = Value::Object(mask);

    // 模板 affinity 里 gtmain.touch 钉在 c3；按实际 cluster 数修正为最后一档
    let last = format!("c{}", clusters.len() - 1);
    cfg["modules"]["sched"]["affinity"]["gtmain"]["touch"] = json!(last);

    // ── 4. GPU ───────────────────────────────────────────────────────────────
    if let Some((min, mid_f, max)) = detect_gpu_freqs() {
        cfg["modules"]["gpu"] = json!({
            "enable": true,
            "minFreqKhz": min,
            "midFreqKhz": mid_f,
            "maxFreqKhz": max,
            "_comment": "auto-detected from devfreq available_frequencies"
        });
        log::info!("platform: auto GPU freqs min={min} mid={mid_f} max={max} kHz");
    } else {
        log::warn!("platform: no GPU devfreq interface found — GPU tuning disabled");
        cfg["modules"]["gpu"]["enable"] = json!(false);
    }

    // ── 5. presets 频率重生成（按 cluster max × 比例，snap 到实际 OPP） ──────
    if let Some(presets) = cfg["presets"].as_object_mut() {
        for (mode, preset) in presets.iter_mut() {
            let Some(preset_obj) = preset.as_object_mut() else { continue };
            // 删除模板遗留的 cNMaxFreq 行
            for (_, params) in preset_obj.iter_mut() {
                if let Some(p) = params.as_object_mut() {
                    p.retain(|k, _| !k.starts_with('c') || !k.ends_with("MaxFreq"));
                }
            }
            // 按 (mode, hint) 写新频率
            for (hint, ratios) in preset_ratios(mode) {
                if let Some(params) = preset_obj.get_mut(hint).and_then(|v| v.as_object_mut()) {
                    for (i, c) in clusters.iter().enumerate() {
                        let r = ratios[i.min(ratios.len() - 1)];
                        let target_khz = (c.max_khz as f64 * r) as u64;
                        let snapped = snap_down(&c.available_khz, target_khz);
                        params.insert(format!("c{i}MaxFreq"), json!(snapped.to_string()));
                    }
                }
            }
        }
    }

    serde_json::from_value(cfg).expect("auto config must be valid")
}

/// 各 (mode, hint) 的频率比例表（按 cluster 索引，超出取最后一个）。
fn preset_ratios(mode: &str) -> Vec<(&'static str, Vec<f64>)> {
    match mode {
        "powersave" => vec![
            ("*",     vec![0.40, 0.55, 0.60, 0.65]),
            ("idle",  vec![0.30, 0.42, 0.48, 0.52]),
            ("switch",vec![0.50, 0.75, 0.80, 0.85]),
        ],
        "balance" => vec![
            ("idle",  vec![0.35, 0.45, 0.50, 0.55]),
            ("switch",vec![0.55, 0.80, 0.85, 0.90]),
        ],
        "performance" => vec![
            ("trigger", vec![1.0, 1.0, 1.0, 1.0]),
            ("switch",  vec![1.0, 1.0, 1.0, 1.0]),
        ],
        "fast" => vec![("*", vec![1.0, 1.0, 1.0, 1.0])],
        _ => vec![],
    }
}

/// 向下 snap 到 OPP 表；表为空时对齐到 96MHz 栅格。
fn snap_down(available: &[u64], target_khz: u64) -> u64 {
    if !available.is_empty() {
        match available.partition_point(|&f| f <= target_khz) {
            0 => available[0],
            n => available[n - 1],
        }
    } else {
        (target_khz / 96000) * 96000
    }
}

// ── per-app 模式覆盖 ──────────────────────────────────────────────────────────

/// 解析 perapp_powermode.txt：每行 `<package> <mode>`。
pub fn load_perapp_map(path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(pkg), Some(mode)) = (it.next(), it.next()) {
                map.insert(pkg.to_string(), mode.to_string());
            }
        }
    }
    map
}
