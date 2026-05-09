// gpu/mod.rs
// MediaTek Mali GPU frequency controller.
//
// 联发科 GPU 调频接口按内核版本分三代，本模块在运行时探测可用路径：
//
// ┌─────────────────────────────────────────────────────────────────────┐
// │ 代际  │ 内核     │ 主要接口                                         │
// ├───────┼──────────┼──────────────────────────────────────────────────┤
// │ Gen1  │ 4.x/5.x  │ /proc/gpufreq/gpufreq_opp_freq  (写入=锁频)     │
// │       │          │ /sys/module/ged/parameters/gpu_dvfs_enable       │
// ├───────┼──────────┼──────────────────────────────────────────────────┤
// │ Gen2  │ 5.x/6.1  │ /sys/kernel/ged/params/...                       │
// │       │          │ /sys/kernel/gpu/gpu_max_clock (ged_ski driver)   │
// │       │          │ /sys/kernel/gpu/gpu_min_clock                    │
// ├───────┼──────────┼──────────────────────────────────────────────────┤
// │ Gen3  │ 6.6+     │ /proc/gpufreq 已移除；只剩 devfreq sysfs         │
// │       │          │ /sys/class/devfreq/*.mali/max_freq               │
// │       │          │ /sys/class/devfreq/*.mali/min_freq               │
// │       │          │ /sys/devices/platform/*.mali/devfreq/*.mali/...  │
// └─────────────────────────────────────────────────────────────────────┘
//
// 场景策略（对应 FSM hint）：
//   idle    → 允许 GPU 下降到最低档，开启 DVFS 自动调频
//   touch   → 设定中等下限，避免首帧低频卡顿
//   boost   → 临时提升下限到中高频，用于 APP 切换动画
//   fast    → 锁定最大频率（performance 模式下的 trigger/switch）

use std::fs;
use std::path::PathBuf;
use crate::fsm::Hint;

// ── GPU 频率配置（来自 platform config JSON 的 gpuModel 段） ───────────────

#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// 最小频率 kHz（idle 下限）
    pub min_freq_khz:  u64,
    /// 中等频率 kHz（touch 下限）
    pub mid_freq_khz:  u64,
    /// 最大频率 kHz（boost/fast 上限）
    pub max_freq_khz:  u64,
}

impl Default for GpuConfig {
    fn default() -> Self {
        // 保守默认值，实际会被平台 JSON 覆盖
        Self { min_freq_khz: 200_000, mid_freq_khz: 450_000, max_freq_khz: 1_000_000 }
    }
}

// ── 接口代际 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum GpuInterface {
    /// Gen1: /proc/gpufreq  +  /sys/module/ged
    ProcGpufreq {
        opp_freq:    PathBuf,   // 写入锁频（0 = 解锁）
        dvfs_enable: PathBuf,   // 1=自动, 0=锁最高
    },
    /// Gen2: ged_ski /sys/kernel/gpu
    GedSki {
        max_clock: PathBuf,     // kHz
        min_clock: PathBuf,     // kHz
    },
    /// Gen3: devfreq sysfs
    Devfreq {
        max_freq: PathBuf,      // Hz (devfreq 用 Hz)
        min_freq: PathBuf,      // Hz
    },
    /// 找不到任何接口
    Unsupported,
}

// ── GpuGovernor ─────────────────────────────────────────────────────────────

pub struct GpuGovernor {
    iface:  GpuInterface,
    cfg:    GpuConfig,
    last_hint: Option<Hint>,
}

impl GpuGovernor {
    pub fn new(cfg: GpuConfig) -> Self {
        let iface = detect_interface();
        match &iface {
            GpuInterface::Unsupported =>
                log::warn!("gpu: no controllable interface found; GPU tuning disabled"),
            other =>
                log::info!("gpu: using interface {:?}", other),
        }
        Self { iface, cfg, last_hint: None }
    }

    /// FSM hint 变化时调用。相同 hint 不重复写。
    pub fn apply_hint(&mut self, hint: Hint) {
        if self.last_hint == Some(hint) { return; }
        self.last_hint = Some(hint);

        match hint {
            Hint::Idle => self.set_idle(),
            Hint::Touch | Hint::Trigger | Hint::Gesture | Hint::Junk => self.set_touch(),
            Hint::Switch => self.set_boost(),
        }
    }

    // ── 场景实现 ────────────────────────────────────────────────────────────

    /// Idle：释放限制，让 DVFS 自动降频省电
    fn set_idle(&self) {
        log::debug!("gpu: idle — release dvfs");
        match &self.iface {
            GpuInterface::ProcGpufreq { opp_freq, dvfs_enable } => {
                write_sysfs(dvfs_enable, "1");   // 开启自动 DVFS
                write_sysfs(opp_freq, "0");       // 解除锁频
            }
            GpuInterface::GedSki { max_clock, min_clock } => {
                write_sysfs(max_clock, &self.cfg.max_freq_khz.to_string());
                write_sysfs(min_clock, &self.cfg.min_freq_khz.to_string());
            }
            GpuInterface::Devfreq { max_freq, min_freq } => {
                let max_hz = self.cfg.max_freq_khz * 1000;
                let min_hz = self.cfg.min_freq_khz * 1000;
                write_sysfs(max_freq, &max_hz.to_string());
                write_sysfs(min_freq, &min_hz.to_string());
            }
            GpuInterface::Unsupported => {}
        }
    }

    /// Touch：提高下限，避免首帧低频
    fn set_touch(&self) {
        log::debug!("gpu: touch — raise floor to {} kHz", self.cfg.mid_freq_khz);
        match &self.iface {
            GpuInterface::ProcGpufreq { dvfs_enable, .. } => {
                // Gen1 没有独立 min_freq，只能靠 dvfs_enable=1 让内核自己调
                // 不锁频，但确保 DVFS 已开启
                write_sysfs(dvfs_enable, "1");
            }
            GpuInterface::GedSki { min_clock, max_clock } => {
                write_sysfs(max_clock, &self.cfg.max_freq_khz.to_string());
                write_sysfs(min_clock, &self.cfg.mid_freq_khz.to_string());
            }
            GpuInterface::Devfreq { max_freq, min_freq } => {
                let max_hz = self.cfg.max_freq_khz * 1000;
                let mid_hz = self.cfg.mid_freq_khz * 1000;
                write_sysfs(max_freq, &max_hz.to_string());
                write_sysfs(min_freq, &mid_hz.to_string());
            }
            GpuInterface::Unsupported => {}
        }
    }

    /// Boost（APP 切换）：短暂提升到最大频率
    fn set_boost(&self) {
        log::debug!("gpu: boost — max freq {} kHz", self.cfg.max_freq_khz);
        match &self.iface {
            GpuInterface::ProcGpufreq { opp_freq, dvfs_enable } => {
                write_sysfs(dvfs_enable, "0");   // 关闭自动 DVFS
                write_sysfs(opp_freq, &self.cfg.max_freq_khz.to_string());
            }
            GpuInterface::GedSki { min_clock, max_clock } => {
                write_sysfs(max_clock, &self.cfg.max_freq_khz.to_string());
                write_sysfs(min_clock, &self.cfg.max_freq_khz.to_string());
            }
            GpuInterface::Devfreq { max_freq, min_freq } => {
                let max_hz = self.cfg.max_freq_khz * 1000;
                write_sysfs(max_freq, &max_hz.to_string());
                write_sysfs(min_freq, &max_hz.to_string());
            }
            GpuInterface::Unsupported => {}
        }
    }
}

// ── 接口探测 ─────────────────────────────────────────────────────────────────

fn detect_interface() -> GpuInterface {
    // Gen1: /proc/gpufreq（内核 5.x 及以下）
    let opp = PathBuf::from("/proc/gpufreq/gpufreq_opp_freq");
    let dvfs = PathBuf::from("/sys/module/ged/parameters/gpu_dvfs_enable");
    if opp.exists() && dvfs.exists() {
        log::debug!("gpu: detected Gen1 /proc/gpufreq");
        return GpuInterface::ProcGpufreq { opp_freq: opp, dvfs_enable: dvfs };
    }

    // Gen2: ged_ski /sys/kernel/gpu（部分厂商内核）
    let max_clk = PathBuf::from("/sys/kernel/gpu/gpu_max_clock");
    let min_clk = PathBuf::from("/sys/kernel/gpu/gpu_min_clock");
    if max_clk.exists() && min_clk.exists() {
        log::debug!("gpu: detected Gen2 ged_ski /sys/kernel/gpu");
        return GpuInterface::GedSki { max_clock: max_clk, min_clock: min_clk };
    }

    // Gen3: devfreq（内核 6.6+，Android 15）
    if let Some(iface) = detect_devfreq() {
        return iface;
    }

    GpuInterface::Unsupported
}

/// 扫描 /sys/class/devfreq/ 找到 Mali GPU devfreq 节点
fn detect_devfreq() -> Option<GpuInterface> {
    let devfreq_dir = PathBuf::from("/sys/class/devfreq");
    if !devfreq_dir.exists() { return None; }

    for entry in fs::read_dir(&devfreq_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        // Mali devfreq 节点名含 "mali" 或 "gpu" 或 "mfg"
        if name.contains("mali") || name.contains("gpu") || name.contains("mfg") {
            let base = entry.path();
            let max_freq = base.join("max_freq");
            let min_freq = base.join("min_freq");
            if max_freq.exists() && min_freq.exists() {
                log::debug!("gpu: detected Gen3 devfreq at {}", base.display());
                return Some(GpuInterface::Devfreq { max_freq, min_freq });
            }
        }
    }

    // 也尝试 /sys/devices/platform/*.mali/devfreq/ 路径
    let platform = PathBuf::from("/sys/devices/platform");
    for entry in fs::read_dir(&platform).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains("mali") || name.contains("gpu") {
            let devfreq_sub = entry.path().join("devfreq");
            if let Ok(sub_entries) = fs::read_dir(&devfreq_sub) {
                for sub in sub_entries.flatten() {
                    let max_freq = sub.path().join("max_freq");
                    let min_freq = sub.path().join("min_freq");
                    if max_freq.exists() && min_freq.exists() {
                        log::debug!("gpu: detected Gen3 devfreq (platform) at {}",
                                    sub.path().display());
                        return Some(GpuInterface::Devfreq { max_freq, min_freq });
                    }
                }
            }
        }
    }

    None
}

// ── 工具函数 ─────────────────────────────────────────────────────────────────

fn write_sysfs(path: &PathBuf, value: &str) {
    if let Err(e) = fs::write(path, value) {
        log::debug!("gpu: write {} ← {} failed: {}", path.display(), value, e);
    } else {
        log::trace!("gpu: {} ← {}", path.display(), value);
    }
}
