// sched/mod.rs
// Context-aware task scheduler.
//
// For each thread matched by a rule, applies:
//   • CPU affinity (sched_setaffinity) — pinning to c0/c1/c2
//   • CFS nice      (setpriority)      — values -20 to 19
//   • RT priority   (sched_setscheduler SCHED_FIFO/RR) — values 1-99
//     Original config encodes RT as "nice-like" values 97-139
//     where 139 = RT prio 1 (lowest RT), 98 = RT prio 41, 97 = RT prio 42
//     Mapping:  rt_prio = 140 - config_value  (matches original uperf convention)
//
// Special thread key "/MAIN_THREAD/" matches the thread whose tid == pid
// (the process main thread).
//
// Scene column selection:
//   Idle → idle, Touch/Trigger/Gesture/Junk → touch, Switch → boost

use std::fs;
use std::collections::HashMap;
use anyhow::Result;
use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;
use regex::Regex;
use crate::config::{SchedModule, ThreadRule};

// ── Scene ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SchedScene {
    Bg,
    Fg,
    Idle,
    Touch,
    Boost,
}

// ── Priority encoding ────────────────────────────────────────────────────────
//
// Config value ranges:
//   negative (-20..-1) → CFS nice, passed directly
//   0                  → "auto" / no change
//   1..96              → CFS nice clamped to 19 (shouldn't appear in practice)
//   97..139            → RT SCHED_FIFO, rt_prio = 140 - value
//                        97 → rt 43, 98 → rt 42, 139 → rt 1

#[derive(Debug, Clone, Copy)]
enum Priority {
    Unchanged,
    Nice(i32),      // CFS, setpriority
    Rt(u32),        // SCHED_FIFO, sched_setscheduler
}

impl Priority {
    fn from_config(v: i32) -> Self {
        match v {
            0             => Priority::Unchanged,
            v if v < 0    => Priority::Nice(v.clamp(-20, 19)),
            v if v >= 97  => Priority::Rt((140 - v).clamp(1, 99) as u32),
            v             => Priority::Nice(v.clamp(-20, 19)),
        }
    }
}

// ── Thread action ─────────────────────────────────────────────────────────────

struct ThreadAction {
    affinity_cpus: Vec<u32>,   // empty = don't touch
    priority:      Priority,
}

// ── TaskScheduler ─────────────────────────────────────────────────────────────

pub struct TaskScheduler {
    cfg:            SchedModule,
    proc_regexes:   Vec<(Regex, usize)>,        // (compiled regex, rule_index)
    thread_regexes: Vec<Vec<(Regex, usize)>>,   // [rule_idx][thread_rule_idx]
    scene:          SchedScene,
    /// Cache: pid → rule_index (invalidated on scene change)
    pid_cache:      HashMap<u32, usize>,
    /// Flag: scene just changed, force full re-apply on next scan
    scene_dirty:    bool,
    /// (tid, affinity bitmask) of every thread the last scan applied an
    /// action to. Used to seed the eBPF APPLIED_MAP so that future forks
    /// of tracked threads are reported to userspace immediately.
    last_seed:      Vec<(u32, u64)>,
}

impl TaskScheduler {
    pub fn new(cfg: SchedModule) -> Self {
        let mut proc_regexes   = Vec::new();
        let mut thread_regexes = Vec::new();

        for (i, rule) in cfg.rules.iter().enumerate() {
            match Regex::new(&rule.regex) {
                Ok(re) => proc_regexes.push((re, i)),
                Err(e) => log::warn!("sched: bad process regex '{}': {}", rule.regex, e),
            }
            let mut tr = Vec::new();
            for (j, trule) in rule.rules.iter().enumerate() {
                // "/MAIN_THREAD/" is a special sentinel, not a real regex
                let key = if trule.k == "/MAIN_THREAD/" { "$^MAIN" } else { &trule.k };
                match Regex::new(key) {
                    Ok(re) => tr.push((re, j)),
                    Err(e) => log::warn!("sched: bad thread regex '{}': {}", trule.k, e),
                }
            }
            thread_regexes.push(tr);
        }

        Self {
            cfg,
            proc_regexes,
            thread_regexes,
            scene:       SchedScene::Idle,
            pid_cache:   HashMap::new(),
            scene_dirty: false,
            last_seed:   Vec::new(),
        }
    }

    pub fn set_scene(&mut self, scene: SchedScene) {
        if self.scene != scene {
            log::debug!("sched: scene → {:?}", scene);
            self.scene       = scene;
            self.scene_dirty = true;
            // Don't clear pid_cache — process→rule mapping is scene-independent
        }
    }

    /// Scan /proc and apply affinity + priority to all matched threads.
    /// When scene_dirty, forces re-apply even if pid was previously seen.
    pub fn scan_and_apply(&mut self) {
        let _ = self.scan_core();
    }

    /// Scan core — same as scan_and_apply but returns the (tid, affinity
    /// bitmask) of every thread an action was applied to.
    fn scan_core(&mut self) -> Vec<(u32, u64)> {
        let force = self.scene_dirty;
        self.scene_dirty = false;
        self.last_seed.clear();

        let procs = match list_procs() {
            Ok(p)  => p,
            Err(e) => { log::warn!("sched: proc scan failed: {e}"); return Vec::new(); }
        };

        // Remove stale pids from cache
        let alive: std::collections::HashSet<u32> = procs.iter().map(|(p,_)| *p).collect();
        self.pid_cache.retain(|pid, _| alive.contains(pid));

        for (pid, cmdline) in &procs {
            let rule_idx = if let Some(&cached) = self.pid_cache.get(pid) {
                cached
            } else if let Some(ridx) = self.match_process(cmdline) {
                self.pid_cache.insert(*pid, ridx);
                ridx
            } else {
                continue;
            };

            // If scene hasn't changed and this pid was already processed,
            // only re-apply for pinned processes (always re-check).
            let pinned = self.cfg.rules.get(rule_idx)
                .map(|r| r.pinned)
                .unwrap_or(false);
            if !force && !pinned { continue; }

            if let Ok(threads) = list_threads(*pid) {
                for (tid, tname) in threads {
                    let is_main = tid == *pid;
                    if let Some(action) = self.resolve_thread(rule_idx, &tname, is_main) {
                        let bits = action.affinity_cpus.iter()
                            .filter(|&&c| c < 64)
                            .fold(0u64, |b, &c| b | (1u64 << c));
                        apply_action(tid, &action);
                        self.last_seed.push((tid, if bits != 0 { bits } else { 1 }));
                    }
                }
            }
        }
        std::mem::take(&mut self.last_seed)
    }

    // ── eBPF event-path API ──────────────────────────────────────────────────

    /// Literal prefixes of the process regexes, for the in-kernel
    /// whitelist (TARGET_COMM_MAP). Regexes that start with a
    /// metacharacter yield no prefix — those processes remain covered by
    /// the periodic /proc scan. Duplicates removed, sorted.
    pub fn whitelist_prefixes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (re, _) in &self.proc_regexes {
            let p = literal_prefix(re.as_str());
            if !p.is_empty() { out.push(p); }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Resolve + apply the rule for one thread — the eBPF event path
    /// (FORK/EXEC/RENAME). Reads /proc/{pid}/cmdline for the process
    /// regex match, then the thread-name rules against `tname`.
    /// Returns (rule_idx, affinity bitmask) when the thread matched a
    /// rule, or None when the process matched nothing / is already gone.
    pub fn apply_thread(&mut self, pid: u32, tid: u32, tname: &str) -> Option<(usize, u64)> {
        let cmdline = read_cmdline(pid)?;
        let rule_idx = self.match_process(&cmdline)?;
        self.pid_cache.insert(pid, rule_idx);
        let action = self.resolve_thread(rule_idx, tname, tid == pid)?;
        let bits = action.affinity_cpus.iter()
            .filter(|&&c| c < 64)
            .fold(0u64, |b, &c| b | (1u64 << c));
        apply_action(tid, &action);
        Some((rule_idx, if bits != 0 { bits } else { 1 }))
    }

    /// Force a full re-apply scan and return the (tid, affinity bits) of
    /// every thread an action was applied to. Used by the eBPF path to
    /// seed APPLIED_MAP so children of tracked threads get events.
    pub fn scan_and_apply_seeded(&mut self) -> Vec<(u32, u64)> {
        self.scene_dirty = true;
        self.scan_core()
    }

    // ── matching ──────────────────────────────────────────────────────────────

    fn match_process(&self, cmdline: &str) -> Option<usize> {
        for (re, ridx) in &self.proc_regexes {
            if re.is_match(cmdline) {
                return Some(*ridx);
            }
        }
        None
    }

    fn resolve_thread(
        &self,
        proc_ridx: usize,
        thread_name: &str,
        is_main_thread: bool,
    ) -> Option<ThreadAction> {
        let trules    = self.thread_regexes.get(proc_ridx)?;
        let proc_rule = self.cfg.rules.get(proc_ridx)?;

        for (re, tidx) in trules {
            let trule = &proc_rule.rules[*tidx];
            let matched = if trule.k == "/MAIN_THREAD/" {
                is_main_thread
            } else {
                re.is_match(thread_name)
            };
            if matched {
                return Some(self.build_action(trule));
            }
        }
        None
    }

    fn build_action(&self, trule: &ThreadRule) -> ThreadAction {
        let affinity_cpus = if trule.ac.is_empty() || trule.ac == "auto" {
            vec![]
        } else {
            self.resolve_cpumask(&trule.ac)
        };

        let priority = if trule.pc.is_empty() || trule.pc == "auto" {
            Priority::Unchanged
        } else {
            self.resolve_priority(&trule.pc)
        };

        ThreadAction { affinity_cpus, priority }
    }

    fn resolve_cpumask(&self, class: &str) -> Vec<u32> {
        let aff = match self.cfg.affinity.get(class) { Some(a) => a, None => return vec![] };
        let mask_name = match self.scene {
            SchedScene::Bg    => &aff.bg,
            SchedScene::Fg    => &aff.fg,
            SchedScene::Idle  => &aff.idle,
            SchedScene::Touch => &aff.touch,
            SchedScene::Boost => &aff.boost,
        };
        if mask_name.is_empty() { return vec![]; }
        self.cfg.cpumask.get(mask_name.as_str()).cloned().unwrap_or_default()
    }

    fn resolve_priority(&self, class: &str) -> Priority {
        let prio = match self.cfg.prio.get(class) {
            Some(p) => p,
            None    => return Priority::Unchanged,
        };
        let v = match self.scene {
            SchedScene::Bg    => prio.bg,
            SchedScene::Fg    => prio.fg,
            SchedScene::Idle  => prio.idle,
            SchedScene::Touch => prio.touch,
            SchedScene::Boost => prio.boost,
        };
        Priority::from_config(v)
    }
}

// ── Syscall application ───────────────────────────────────────────────────────

fn apply_action(tid: u32, action: &ThreadAction) {
    if !action.affinity_cpus.is_empty() {
        let mut set = CpuSet::new();
        for &cpu in &action.affinity_cpus {
            let _ = set.set(cpu as usize);
        }
        if let Err(e) = sched_setaffinity(Pid::from_raw(tid as i32), &set) {
            log::trace!("sched_setaffinity tid={tid} failed: {e}");
        }
    }

    match action.priority {
        Priority::Unchanged => {}
        Priority::Nice(n) => {
            let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, n) };
            if ret != 0 {
                log::trace!("setpriority tid={tid} nice={n} failed: errno={}",
                    std::io::Error::last_os_error());
            }
        }
        Priority::Rt(rt_prio) => {
            // Switch to SCHED_FIFO with the given real-time priority
            let param = libc::sched_param { sched_priority: rt_prio as i32 };
            let ret = unsafe {
                libc::sched_setscheduler(tid as i32, libc::SCHED_FIFO, &param)
            };
            if ret != 0 {
                // Fall back to high nice if RT isn't permitted
                let fallback_nice = -10i32;
                unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, fallback_nice); }
                log::trace!("sched_setscheduler RT tid={tid} prio={rt_prio} failed, \
                             fell back to nice={fallback_nice}");
            }
        }
    }
}

// ── /proc helpers ─────────────────────────────────────────────────────────────

/// Read /proc/{pid}/cmdline with NULs replaced by spaces (same
/// normalization the periodic scan uses).
fn read_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
    let cmdline = raw.replace('\0', " ").trim().to_string();
    (!cmdline.is_empty()).then_some(cmdline)
}

/// Longest literal prefix of a regex string, for eBPF whitelist keys.
/// Stops at the first metacharacter; escaped punctuation (\. \\ \/ …)
/// counts as a literal, character classes (\d \w …) terminate.
/// e.g. `com\.tencent\.mm` → `com.tencent.mm`, `.*foo` → `` (empty).
fn literal_prefix(regex: &str) -> String {
    let mut out = String::new();
    let mut chars = regex.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) if n.is_ascii_punctuation() => out.push(n),
                _ => break,
            },
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' => break,
            c => out.push(c),
        }
    }
    out
}

fn list_procs() -> Result<Vec<(u32, String)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc")?.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if let Ok(pid) = s.parse::<u32>() {
            if let Some(cmdline) = read_cmdline(pid) {
                out.push((pid, cmdline));
            }
        }
    }
    Ok(out)
}

fn list_threads(pid: u32) -> Result<Vec<(u32, String)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/task"))?.flatten() {
        if let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() {
            let comm = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
                .unwrap_or_default()
                .trim()
                .to_string();
            out.push((tid, comm));
        }
    }
    Ok(out)
}
