// main.rs
// uperf — userspace performance controller for Android
// Rust rewrite, compatible with original yc9559/uperf JSON config format.
//
// Architecture:
//   epoll event loop
//     ├── inotify fd  (cpuset, wakelock, mode-switch file)  → WatchEvent
//     ├── input event fds (/dev/input/event*)              → TouchEvent
//     └── timerfd (40 ms)                                  → sample/tick
//
//   Events drive the FSM → on state change:
//     • SysfsWriter applies the preset knobs
//     • Governor recalculates CPU frequency limits
//     • TaskScheduler updates thread affinity/priority

mod config;
mod fsm;
mod input;
mod sysfs;
mod cpu;
mod sched;
mod watcher;
mod gpu;
mod platform;
mod ebpf;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use libc::{epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLL_CTL_ADD};

use config::Config;
use fsm::{Hint, StateMachine};
use input::{InputReader, TouchEvent};
use sysfs::SysfsWriter;
use cpu::{Governor, PowerModel};
use sched::{TaskScheduler, SchedScene};
use watcher::{Watcher, WatchEvent};
use gpu::{GpuGovernor, GpuConfig};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "uperf", about = "Userspace performance controller for Android")]
struct Cli {
    /// Path to the JSON config file
    config: String,

    /// Log output file path (default: stderr)
    #[arg(short = 'o', long)]
    output: Option<String>,
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = std::env::var("UPERF_LOG").unwrap_or_else(|_| "info".into());
    let mut log_builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&log_level)
    );
    if let Some(path) = &cli.output {
        // Append, never truncate — the daemon is restarted by service.sh
        // across reboots and the history matters.
        match std::fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => {
                log_builder.target(env_logger::Target::Pipe(Box::new(f)));
            }
            Err(e) => eprintln!(
                "uperf: cannot open log file {path} ({e}) — logging to stderr"
            ),
        }
    }
    log_builder.init();

    log::info!("uperf starting — config: {}", cli.config);

    // Load config; on failure (or an explicit "auto" marker) build one from
    // runtime platform detection so unknown SoCs still get a sane profile.
    let cfg = match Config::load(&cli.config) {
        Ok(c) if c.meta.name != "auto" => c,
        Ok(c) => {
            log::warn!("config '{}' marked auto — building from runtime detection", c.meta.name);
            platform::build_auto_config()
        }
        Err(e) => {
            log::warn!("config load failed ({e}) — building from runtime detection");
            platform::build_auto_config()
        }
    };
    log::info!("Platform: {}", cfg.meta.name);

    // --- Build cluster layout ----------------------------------------------
    let mut cluster_keys: Vec<String> = cfg.modules.sched.cpumask.keys()
        .filter(|k| k.starts_with('c') && k[1..].parse::<u32>().is_ok())
        .cloned()
        .collect();
    cluster_keys.sort_by_key(|k| k[1..].parse::<u32>().unwrap_or(99));

    let cluster_leaders: Vec<u32> = cluster_keys.iter()
        .filter_map(|k| cfg.modules.sched.cpumask.get(k)?.first().copied())
        .collect();

    let all_cpus: Vec<u32> = cfg.modules.sched.cpumask
        .get("all")
        .cloned()
        .unwrap_or_else(|| (0u32..8).collect());

    // --- Subsystems --------------------------------------------------------
    let mut sysfs = SysfsWriter::new(
        &cfg.modules.sysfs.knob,
        cluster_leaders,
        all_cpus,
    );

    // Apply sysfs initials
    for (k, v) in &cfg.initials.sysfs {
        sysfs.write(k, v);
    }

    let power_model = PowerModel::new(&cfg.modules.cpu, &cfg.modules.sched.cpumask);
    let mut governor = Governor::new(power_model, &cfg.initials.cpu);
    let gpu_cfg = GpuConfig {
        min_freq_khz: cfg.modules.gpu.min_freq_khz,
        mid_freq_khz: cfg.modules.gpu.mid_freq_khz,
        max_freq_khz: cfg.modules.gpu.max_freq_khz,
    };
    let mut gpu_gov = if cfg.modules.gpu.enable {
        Some(GpuGovernor::new(gpu_cfg))
    } else {
        log::info!("GPU governor disabled in config");
        None
    };
    let mut scheduler = TaskScheduler::new(cfg.modules.sched.clone());

    // eBPF event-driven acceleration for the TaskScheduler (optional).
    // Loads uperf-ebpf from the binary's directory; every failure path
    // (no BTF, missing object, attach error) falls back to the classic
    // 200 ms /proc scan — zero behavior change on unsupported devices.
    let mut ebpf_state = if cfg.modules.sched.enable {
        ebpf::init(&scheduler)
    } else {
        None
    };
    if ebpf_state.is_some() {
        // Enable the comm cache BEFORE the seed scan so the seed populates
        // it — later scans then read comms only for unseen tids.
        scheduler.set_ebpf_mode(true);
    }
    if let Some(ref mut es) = ebpf_state {
        // One forced scan so APPLIED_MAP knows every currently-tracked
        // thread (their future forks then arrive as kernel events).
        es.seed_from_scan(&mut scheduler);
    }

    let mut fsm = StateMachine::new(cfg.modules.switcher.hint_duration.clone());

    let mut input_reader = if cfg.modules.input.enable {
        match InputReader::new(cfg.modules.input.clone()) {
            Ok(r)  => Some(r),
            Err(e) => { log::warn!("Input reader unavailable: {e}"); None }
        }
    } else {
        None
    };

    let mut watcher = Watcher::new(&cfg.modules.switcher.switch_inode)?;

    // Initial performance mode
    let mut current_mode = read_mode_file(&cfg.modules.switcher.switch_inode);
    if current_mode == "auto" || !cfg.presets.contains_key(&current_mode) {
        current_mode = cfg.presets.keys().next().cloned()
            .unwrap_or_else(|| "balance".into());
    }
    log::info!("Initial performance mode: {}", current_mode);
    apply_preset(&mut sysfs, &mut governor, &cfg, &current_mode, fsm.current());

    // --- epoll setup -------------------------------------------------------
    let epfd = unsafe { epoll_create1(0) };
    if epfd < 0 { return Err(io::Error::last_os_error()).context("epoll_create1"); }

    epoll_add(epfd, watcher.fd());

    if let Some(ref ir) = input_reader {
        for fd in ir.fds() { epoll_add(epfd, fd); }
    }

    let timer_fd = create_timerfd(Duration::from_millis(40))?;
    epoll_add(epfd, timer_fd);

    log::info!("Event loop started");

    // --- Event loop --------------------------------------------------------
    let mut events = [epoll_event { events: 0, u64: 0 }; 32];
    let mut last_sched_scan = Instant::now();

    // Self-profiling state — every 5 s one `perf:` log line with per-thread
    // CPU of this process plus the cost breakdown of the hot paths. This is
    // the ground truth for idle-CPU investigations (a 15 % static draw can
    // otherwise only be guessed at).
    let mut perf_window_start = Instant::now();
    let mut perf_threads: HashMap<u32, u64> = HashMap::new();
    let mut perf_scan_n = 0u64;
    let mut perf_scan_ns: u128 = 0;
    let mut perf_scan_max_ns: u128 = 0;
    let mut perf_gov_n = 0u64;
    let mut perf_gov_ns: u128 = 0;
    let mut perf_gov_max_ns: u128 = 0;
    let mut perf_ev_last: u64 = 0;

    // Screen-state fallback (wake_lock string matching is unreliable on
    // some devices): top-app cgroup going empty for 5s ⇒ screen off.
    let mut last_screen_check = Instant::now();
    let mut screen_empty_since: Option<Instant> = None;
    let mut screen_off_active = platform::top_app_empty();

    // Per-app power mode overrides (perapp_powermode.txt)
    let mut last_perapp_check = Instant::now();
    let mut last_pkg = String::new();
    let mut perapp_override = false;

    let mut perf_ev_last: u64 = 0;
    let mut perf_loops: u64 = 0;

    loop {
        perf_loops += 1;
        // ── eBPF process events (non-blocking drain) ──
        if let Some(ref mut es) = ebpf_state {
            es.drain(&mut scheduler);
        }

        let timeout_ms = fsm.next_timeout().as_millis().min(200) as i32;

        // ── screen state fallback (every 500 ms) ──
        if last_screen_check.elapsed() >= Duration::from_millis(500) {
            last_screen_check = Instant::now();
            let empty = platform::top_app_empty();
            if empty {
                if screen_empty_since.is_none() {
                    screen_empty_since = Some(Instant::now());
                }
                if !screen_off_active
                    && screen_empty_since.map(|t| t.elapsed() >= Duration::from_secs(5)).unwrap_or(false)
                {
                    screen_off_active = true;
                    log::info!("screen: off (top-app empty for 5s)");
                    if let Some(h) = fsm.on_screen_off() {
                        on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler,
                                       &cfg, &current_mode, h);
                    }
                }
            } else {
                screen_empty_since = None;
                if screen_off_active {
                    screen_off_active = false;
                    log::info!("screen: on (top-app active)");
                    if let Some(h) = fsm.on_screen_on() {
                        on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler,
                                       &cfg, &current_mode, h);
                    }
                }
            }
        }

        // ── per-app power mode (every 1 s) ──
        if last_perapp_check.elapsed() >= Duration::from_millis(1000) {
            last_perapp_check = Instant::now();
            if let Some(pkg) = platform::top_app_package() {
                if pkg != last_pkg {
                    last_pkg = pkg.clone();
                    let perapp = platform::load_perapp_map(&cfg.modules.switcher.perapp);
                    let next = if let Some(mode) = perapp.get(&pkg) {
                        perapp_override = true;
                        Some(mode.clone())
                    } else if perapp_override {
                        // Left the per-app app — fall back to the manual mode file
                        perapp_override = false;
                        let manual = read_mode_file(&cfg.modules.switcher.switch_inode);
                        Some(if manual == "auto" || !cfg.presets.contains_key(&manual) {
                            cfg.presets.keys().next().cloned().unwrap_or_else(|| "balance".into())
                        } else {
                            manual
                        })
                    } else {
                        None
                    };
                    if let Some(mode) = next {
                        if cfg.presets.contains_key(&mode) && mode != current_mode {
                            log::info!("perapp: foreground '{pkg}' → mode '{mode}'");
                            current_mode = mode;
                            governor.force_write_next();
                            apply_preset(&mut sysfs, &mut governor, &cfg,
                                         &current_mode, fsm.current());
                        }
                    }
                }
            }
        }

        let n = unsafe {
            epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, timeout_ms)
        };

        // FSM timeout tick
        if let Some(new_hint) = fsm.tick() {
            on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler,
                           &cfg, &current_mode, new_hint);
        }

        if n <= 0 { continue; }

        for ev in &events[..n as usize] {
            let fd = ev.u64 as RawFd;

            if fd == watcher.fd() {
                for we in watcher.drain() {
                    match we {
                        WatchEvent::AppSwitch => {
                            if let Some(h) = fsm.on_window_switch() {
                                on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ScreenOn => {
                            if let Some(h) = fsm.on_screen_on() {
                                on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ScreenOff => {
                            if let Some(h) = fsm.on_screen_off() {
                                on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ModeChange(mode) => {
                            log::info!("Mode: {} → {}", current_mode, mode);
                            current_mode = if mode == "auto" || !cfg.presets.contains_key(&mode) {
                                cfg.presets.keys().next().cloned().unwrap_or(mode)
                            } else {
                                mode
                            };
                            governor.force_write_next();
                            apply_preset(&mut sysfs, &mut governor,
                                         &cfg, &current_mode, fsm.current());
                        }
                    }
                }
                continue;
            }

            if fd == timer_fd {
                let mut buf = [0u8; 8];
                let _ = unsafe { libc::read(timer_fd, buf.as_mut_ptr() as _, 8) };
                let t0 = Instant::now();
                governor.apply(&mut sysfs, screen_off_active);
                let gov_ns = t0.elapsed().as_nanos();
                perf_gov_n += 1;
                perf_gov_ns += gov_ns;
                if gov_ns > perf_gov_max_ns { perf_gov_max_ns = gov_ns; }
                if last_sched_scan.elapsed() >= Duration::from_millis(200) {
                    let t1 = Instant::now();
                    scheduler.scan_and_apply();
                    let scan_ns = t1.elapsed().as_nanos();
                    perf_scan_n += 1;
                    perf_scan_ns += scan_ns;
                    if scan_ns > perf_scan_max_ns { perf_scan_max_ns = scan_ns; }
                    last_sched_scan = Instant::now();
                }

                // ── periodic self-profile (every 5 s) ──
                if perf_window_start.elapsed() >= Duration::from_secs(5) {
                    let window = perf_window_start.elapsed().as_secs_f64().max(0.001);
                    perf_window_start = Instant::now();

                    // Per-thread CPU of this process from /proc jiffies.
                    let mut delta_total = 0u64;
                    let mut parts: Vec<String> = Vec::new();
                    for (tid, comm, jiffies) in thread_cpu_jiffies() {
                        let prev = perf_threads.insert(tid, jiffies).unwrap_or(jiffies);
                        let d = jiffies.saturating_sub(prev);
                        if d == 0 { continue; }
                        delta_total += d;
                        // USER_HZ = 100 on Android/bionic → 100 jiffies = 1 core-second.
                        let pct = d as f64 / (window * 100.0);
                        if pct >= 0.3 {
                            parts.push(format!("{comm}:{tid} {pct:.1}%"));
                        }
                    }

                    let ev_now = ebpf_state.as_ref().map(|e| e.events).unwrap_or(0);
                    let ev_rate = ev_now.saturating_sub(perf_ev_last) as f64 / window;
                    perf_ev_last = ev_now;

                    log::info!(
                        "perf: {:.1}% total [{}] | scan avg {:.2}ms max {:.2}ms n={} | \
                         gov avg {:.2}ms max {:.2}ms n={} | ebpf {:.0} ev/s | loop {:.0}/s",
                        delta_total as f64 / (window * 100.0),
                        if parts.is_empty() { "idle".to_string() } else { parts.join(", ") },
                        perf_scan_ns as f64 / perf_scan_n.max(1) as f64 / 1e6,
                        perf_scan_max_ns as f64 / 1e6,
                        perf_scan_n,
                        perf_gov_ns as f64 / perf_gov_n.max(1) as f64 / 1e6,
                        perf_gov_max_ns as f64 / 1e6,
                        perf_gov_n,
                        ev_rate,
                        perf_loops as f64 / window,
                    );
                    perf_scan_n = 0; perf_scan_ns = 0; perf_scan_max_ns = 0;
                    perf_gov_n = 0; perf_gov_ns = 0; perf_gov_max_ns = 0;
                    perf_loops = 0;
                }
                continue;
            }

            if let Some(ref mut ir) = input_reader {
                for touch_ev in ir.read_events(fd) {
                    let new_hint = match touch_ev {
                        TouchEvent::Down    => fsm.on_touch_down(),
                        TouchEvent::Up      => fsm.on_touch_up(),
                        TouchEvent::Swipe   => fsm.on_swipe(),
                        TouchEvent::Gesture => fsm.on_gesture(),
                    };
                    if let Some(h) = new_hint {
                        on_hint_change(&mut sysfs, &mut governor, &mut gpu_gov, &mut scheduler, &cfg, &current_mode, h);
                    }
                }
            }
        }
    }
}

// ── Hint change ──────────────────────────────────────────────────────────────

/// (tid, comm, utime+stime in jiffies) for every thread of this process.
/// Used by the 5 s self-profile line. /proc/[pid]/stat fields after the
/// comm (which may itself contain spaces/parens — split after the last
/// ')'): state=3 … utime=14, stime=15, i.e. offsets 11 and 12 past the
/// closing paren.
fn thread_cpu_jiffies() -> Vec<(u32, String, u64)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc/self/task") {
        for entry in rd.flatten() {
            let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(raw) = fs::read_to_string(format!("/proc/self/task/{tid}/stat")) else { continue };
            let Some((comm, rest)) = raw.rsplit_once(')') else { continue };
            let comm = comm.rsplit('(').next().unwrap_or("").to_string();
            let f: Vec<&str> = rest.split_whitespace().collect();
            let jiff = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            out.push((tid, comm, jiff(11) + jiff(12)));
        }
    }
    out
}

fn on_hint_change(
    sysfs:     &mut SysfsWriter,
    governor:  &mut Governor,
    gpu_gov:   &mut Option<GpuGovernor>,
    scheduler: &mut TaskScheduler,
    cfg:       &Config,
    mode:      &str,
    hint:      Hint,
) {
    log::debug!("Hint → {:?}", hint);
    apply_preset(sysfs, governor, cfg, mode, hint);
    if let Some(ref mut g) = gpu_gov { g.apply_hint(hint); }
    let scene = match hint.sched_scene() {
        "boost" => SchedScene::Boost,
        "touch" => SchedScene::Touch,
        _       => SchedScene::Idle,
    };
    scheduler.set_scene(scene);
}

// ── Preset application ────────────────────────────────────────────────────────

fn apply_preset(
    sysfs:    &mut SysfsWriter,
    governor: &mut Governor,
    cfg:      &Config,
    mode:     &str,
    hint:     Hint,
) {
    let preset = match cfg.presets.get(mode) {
        Some(p) => p,
        None    => { log::warn!("Unknown preset '{mode}'"); return; }
    };

    let mut params: HashMap<String, String> = HashMap::new();
    if let Some(global) = preset.get("*") {
        for (k, v) in global { params.insert(k.clone(), json_to_str(v)); }
    }
    if let Some(hp) = preset.get(hint.as_str()) {
        for (k, v) in hp { params.insert(k.clone(), json_to_str(v)); }
    }

    let mut sysfs_params = HashMap::new();
    for (k, v) in &params {
        if let Some(cpu_key) = k.strip_prefix("cpu.") {
            apply_cpu_param(governor, cpu_key, v);
        } else if let Some(sysfs_key) = k.strip_prefix("sysfs.") {
            sysfs_params.insert(sysfs_key.to_string(), v.clone());
        } else if !k.starts_with("sched.") {
            sysfs_params.insert(k.clone(), v.clone());
        }
    }
    sysfs.apply_batch(&sysfs_params);
}

fn apply_cpu_param(governor: &mut Governor, key: &str, value: &str) {
    match key {
        "fastLimitPower" | "fast_limit_power" => {
            if let Ok(v) = value.parse::<f64>() { governor.max_power = v; }
        }
        "margin" => {
            if let Ok(v) = value.parse::<f64>() { governor.margin = v; }
        }
        "limitEfficiency" | "limit_efficiency" => {
            governor.limit_eff = matches!(value, "true" | "1");
        }
        _ => {}
    }
}

fn json_to_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b)   => b.to_string(),
        other                        => other.to_string(),
    }
}

// ── epoll / timerfd helpers ───────────────────────────────────────────────────

fn epoll_add(epfd: RawFd, fd: RawFd) {
    let mut ev = epoll_event { events: EPOLLIN as u32, u64: fd as u64 };
    unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev); }
}

fn create_timerfd(interval: Duration) -> Result<RawFd> {
    let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK) };
    if fd < 0 { return Err(io::Error::last_os_error()).context("timerfd_create"); }
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: interval.as_secs() as i64,
            tv_nsec: interval.subsec_nanos() as i64,
        },
        it_value: libc::timespec { tv_sec: 0, tv_nsec: 1 },
    };
    unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()); }
    Ok(fd)
}

// ── Misc ──────────────────────────────────────────────────────────────────────

fn read_mode_file(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("balance")
        .to_lowercase()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_cpu_jiffies_parses_real_proc() {
        let threads = thread_cpu_jiffies();
        assert!(threads.len() >= 1, "must at least see the calling thread");
        let pid = unsafe { libc::getpid() } as u32;
        assert!(threads.iter().any(|(tid, _, _)| *tid == pid),
                "main thread tid={pid} must be listed, got {:?}", threads);
        // Cumulative jiffies of a short-lived test process stay tiny; a
        // parsing bug (wrong field offset) would show up as 0 or absurd.
        for (_, _, j) in &threads {
            assert!(*j < 100_000, "implausible jiffies {j}");
        }
    }
}
