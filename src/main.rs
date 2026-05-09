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

use std::collections::HashMap;
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
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(&log_level)
    ).init();

    log::info!("uperf starting — config: {}", cli.config);

    let cfg = Config::load(&cli.config)?;
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
    let mut scheduler = TaskScheduler::new(cfg.modules.sched.clone());
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

    loop {
        let timeout_ms = fsm.next_timeout().as_millis().min(200) as i32;
        let n = unsafe {
            epoll_wait(epfd, events.as_mut_ptr(), events.len() as i32, timeout_ms)
        };

        // FSM timeout tick
        if let Some(new_hint) = fsm.tick() {
            on_hint_change(&mut sysfs, &mut governor, &mut scheduler,
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
                                on_hint_change(&mut sysfs, &mut governor,
                                               &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ScreenOn => {
                            if let Some(h) = fsm.on_screen_on() {
                                on_hint_change(&mut sysfs, &mut governor,
                                               &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ScreenOff => {
                            if let Some(h) = fsm.on_screen_off() {
                                on_hint_change(&mut sysfs, &mut governor,
                                               &mut scheduler, &cfg, &current_mode, h);
                            }
                        }
                        WatchEvent::ModeChange(mode) => {
                            log::info!("Mode: {} → {}", current_mode, mode);
                            current_mode = if mode == "auto" || !cfg.presets.contains_key(&mode) {
                                cfg.presets.keys().next().cloned().unwrap_or(mode)
                            } else {
                                mode
                            };
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
                governor.apply(&mut sysfs);
                if last_sched_scan.elapsed() >= Duration::from_millis(200) {
                    scheduler.scan_and_apply();
                    last_sched_scan = Instant::now();
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
                        on_hint_change(&mut sysfs, &mut governor,
                                       &mut scheduler, &cfg, &current_mode, h);
                    }
                }
            }
        }
    }
}

// ── Hint change ──────────────────────────────────────────────────────────────

fn on_hint_change(
    sysfs:     &mut SysfsWriter,
    governor:  &mut Governor,
    scheduler: &mut TaskScheduler,
    cfg:       &Config,
    mode:      &str,
    hint:      Hint,
) {
    log::debug!("Hint → {:?}", hint);
    apply_preset(sysfs, governor, cfg, mode, hint);
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
