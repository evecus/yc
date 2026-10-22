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
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use anyhow::Result;
use nix::sched::{sched_getaffinity, sched_setaffinity, CpuSet};
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

// ── Scan pacing ──────────────────────────────────────────────────────────────

/// Scan period while the scene is active (touch/boost) or a scene change is
/// pending — this is where pinned-thread "escape" re-pinning matters.
const ACTIVE_SCAN_INTERVAL: Duration = Duration::from_millis(200);
/// Scan period while the device sits in the Idle scene. Nothing is moving, so
/// re-checking ~500 pinned threads at 5 Hz only burns CPU.
const IDLE_SCAN_INTERVAL: Duration = Duration::from_millis(1000);
/// How long a pid with an empty / unmatched cmdline (kernel threads, zombies)
/// stays in the negative cache before it is probed again.
const NEG_CACHE_TTL: Duration = Duration::from_secs(5);

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
    /// tid → last-seen comm. In eBPF mode the periodic scan reuses these
    /// instead of re-opening /proc/{tid}/comm for every thread every scan
    /// (rename events keep the cache fresh; see note_comm/forget_thread).
    comm_cache:     HashMap<u32, String>,
    /// True once the eBPF event path is live — enables the comm cache.
    ebpf_active:    bool,
    /// pid → time it was found unmatchable (empty cmdline = kernel thread /
    /// zombie, or no process rule matched). Without this every scan re-opens
    /// /proc/<pid>/cmdline for every kernel thread (hundreds of pids).
    neg_cache:      HashMap<u32, Instant>,
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
            comm_cache:  HashMap::new(),
            ebpf_active: false,
            neg_cache:   HashMap::new(),
        }
    }

    /// Whether the periodic scan should run now. Idle scene with nothing
    /// pending scans at 1 Hz; anything else keeps the 200 ms cadence.
    pub fn scan_due(&self, since_last: Duration) -> bool {
        let interval = if self.scene_dirty || self.scene != SchedScene::Idle {
            ACTIVE_SCAN_INTERVAL
        } else {
            IDLE_SCAN_INTERVAL
        };
        since_last >= interval
    }

    /// Called once the eBPF event path is live. Enables the comm cache:
    /// the periodic scan then reuses cached comms (refreshed by rename
    /// events) instead of re-reading /proc/{tid}/comm for every thread.
    pub fn set_ebpf_mode(&mut self, on: bool) {
        self.ebpf_active = on;
        if !on { self.comm_cache.clear(); }
    }

    /// eBPF RENAME/EXEC event: refresh the cached comm for `tid`.
    pub fn note_comm(&mut self, tid: u32, comm: &str) {
        if self.ebpf_active {
            self.comm_cache.insert(tid, comm.to_string());
        }
    }

    /// eBPF EXIT event (or scan miss): drop bookkeeping for `tid`.
    pub fn forget_thread(&mut self, tid: u32) {
        self.comm_cache.remove(&tid);
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
    /// CPU-cost notes (this runs every 200 ms):
    ///   • cmdline is read ONLY for pids missing from pid_cache — a full
    ///     /proc cmdline sweep every scan costs ~600 opens and dominated
    ///     the daemon's CPU on slow-procfs devices (measured ~19% idle).
    ///   • in eBPF mode, thread comms come from comm_cache (kept fresh by
    ///     rename events); comm files are opened only for unseen tids.
    fn scan_core(&mut self) -> Vec<(u32, u64)> {
        let force = self.scene_dirty;
        self.scene_dirty = false;
        self.last_seed.clear();

        // Enumerate pids without touching cmdline yet.
        let mut pids: Vec<u32> = Vec::new();
        if let Ok(rd) = fs::read_dir("/proc") {
            for entry in rd.flatten() {
                if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() {
                    pids.push(pid);
                }
            }
        }

        // Remove stale pids from the caches
        let alive: HashSet<u32> = pids.iter().copied().collect();
        self.pid_cache.retain(|pid, _| alive.contains(pid));
        let now = Instant::now();
        self.neg_cache.retain(|pid, t| {
            alive.contains(pid) && now.duration_since(*t) < NEG_CACHE_TTL
        });

        let mut seen_tids: HashSet<u32> = HashSet::new();

        for pid in &pids {
            let rule_idx = if let Some(&cached) = self.pid_cache.get(pid) {
                cached
            } else if self.neg_cache.contains_key(pid) {
                continue;
            } else if let Some(cmdline) = read_cmdline(*pid) {
                match self.match_process(&cmdline) {
                    Some(ridx) => { self.pid_cache.insert(*pid, ridx); ridx }
                    None => { self.neg_cache.insert(*pid, now); continue; }
                }
            } else {
                // Empty cmdline: kernel thread / zombie. Remember it.
                self.neg_cache.insert(*pid, now);
                continue;
            };

            // If scene hasn't changed and this pid was already processed,
            // only re-apply for pinned processes (always re-check).
            let pinned = self.cfg.rules.get(rule_idx)
                .map(|r| r.pinned)
                .unwrap_or(false);
            if !force && !pinned { continue; }

            if let Ok(threads) = self.list_threads_cached(*pid, &mut seen_tids) {
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

        // Evict comm-cache entries for tids no longer observable. Only tids
        // of processes scanned this round are visible; stale entries are
        // simply re-read when their process is scanned again.
        if self.ebpf_active {
            self.comm_cache.retain(|tid, _| seen_tids.contains(tid));
        }
        std::mem::take(&mut self.last_seed)
    }

    /// Threads of `pid` as (tid, comm). In eBPF mode comms are served from
    /// comm_cache and only unseen tids hit /proc; the fallback path reads
    /// every comm fresh. Seen tids are appended to `seen` for cache
    /// eviction bookkeeping.
    fn list_threads_cached(
        &mut self,
        pid: u32,
        seen: &mut HashSet<u32>,
    ) -> Result<Vec<(u32, String)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(format!("/proc/{pid}/task"))?.flatten() {
            if let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() {
                seen.insert(tid);
                if self.ebpf_active {
                    if let Some(c) = self.comm_cache.get(&tid) {
                        out.push((tid, c.clone()));
                        continue;
                    }
                }
                let comm = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if self.ebpf_active {
                    self.comm_cache.insert(tid, comm.clone());
                }
                out.push((tid, comm));
            }
        }
        Ok(out)
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
        self.neg_cache.remove(&pid);
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

/// Apply one thread action, skipping syscalls whose effect is already in
/// place. The 200 ms scan re-applies pinned threads every round; without
/// change detection that is ~750 sched syscalls per scan on a busy device.
/// Reads (getaffinity / getpriority / getparam) are far cheaper than the
/// writes they guard, and sched_setscheduler in particular dequeues and
/// re-queues the task, so skipping no-op writes is a real win. It also
/// preserves the "pinned" escape defense: a thread that dropped out of its
/// affinity is re-pinned on the next scan because the read still mismatches.
fn apply_action(tid: u32, action: &ThreadAction) {
    if !action.affinity_cpus.is_empty() {
        let mut set = CpuSet::new();
        for &cpu in &action.affinity_cpus {
            let _ = set.set(cpu as usize);
        }
        // Skip when the thread already runs on exactly this mask. A dead
        // thread (ESRCH) also reports mismatch-free: getaffinity fails,
        // we attempt the write once, it fails, trace-logged, done.
        let already = sched_getaffinity(Pid::from_raw(tid as i32))
            .map(|cur| cur == set)
            .unwrap_or(false);
        if !already {
            if let Err(e) = sched_setaffinity(Pid::from_raw(tid as i32), &set) {
                log::trace!("sched_setaffinity tid={tid} failed: {e}");
            }
        }
    }

    match action.priority {
        Priority::Unchanged => {}
        Priority::Nice(n) => {
            // getpriority returns -1 both on error and for nice == -1, and
            // -1 is a valid nice value — so only skip the write on an
            // unambiguous match (keeps this portable across glibc/bionic;
            // no errno games). The ambiguous cases fall through to one
            // harmless write.
            let cur = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
            if cur != -1 && cur == n {
                return;
            }
            let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, n) };
            if ret != 0 {
                log::trace!("setpriority tid={tid} nice={n} failed: errno={}",
                    std::io::Error::last_os_error());
            }
        }
        Priority::Rt(rt_prio) => {
            // Read-verify both the policy and the RT priority; skip the
            // (expensive) setscheduler when the thread is already
            // SCHED_FIFO at the wanted priority.
            let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
            let policy = unsafe { libc::sched_getscheduler(tid as i32) };
            let got = unsafe { libc::sched_getparam(tid as i32, &mut param) };
            let already = policy == libc::SCHED_FIFO
                && got == 0
                && param.sched_priority == rt_prio as i32;
            if already {
                return;
            }
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



// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests in this binary share one process, and apply_action pins the
    /// *whole* process affinity — serialize the affinity-touching tests so
    /// parallel execution can't interleave pins and masks.
    static AFFINITY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Scheduler whose first rule matches this test binary by name (cmdline
    /// = path to the test harness) and pins every thread to little cores.
    fn self_sched() -> TaskScheduler {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        // Double-escape: regex-escape so the name is a literal regex, then
        // JSON-escape so the backslashes survive serde_json's parser.
        let rx = serde_json::to_string(&regex::escape(&name)).unwrap();
        let json = format!(r#"{{
            "enable": true,
            "cpumask": {{ "little": [0,1,2,3], "big": [4,5,6,7] }},
            "affinity": {{
                "a_little": {{"bg":"little","fg":"little","idle":"little","touch":"little","boost":"little"}},
                "a_big":    {{"bg":"big","fg":"big","idle":"big","touch":"big","boost":"big"}}
            }},
            "prio": {{
                "p_norm": {{"bg":0,"fg":0,"idle":0,"touch":0,"boost":0}},
                "p_bg":   {{"bg":19,"fg":19,"idle":19,"touch":19,"boost":19}}
            }},
            "rules": [
                {{"name":"self","regex":{},"pinned":true,
                  "rules":[{{"k":".","ac":"a_little","pc":"p_norm"}}]}},
                {{"name":"all","regex":".","pinned":false,
                  "rules":[{{"k":"/MAIN_THREAD/","ac":"a_big","pc":"p_bg"}}]}}
            ]
        }}"#, rx);
        TaskScheduler::new(serde_json::from_str(&json).unwrap())
    }

    /// Like self_sched, but the self-rule affinity flips with the scene:
    /// idle → little, touch → big. Used to prove the read-check inside
    /// apply_action does not permanently suppress legitimate re-pins.
    fn scene_sched() -> TaskScheduler {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        let rx = serde_json::to_string(&regex::escape(&name)).unwrap();
        let json = format!(r#"{{
            "enable": true,
            "cpumask": {{ "little": [0,1,2,3], "big": [4,5,6,7] }},
            "affinity": {{
                "a_flip": {{"bg":"little","fg":"little","idle":"little","touch":"big","boost":"little"}}
            }},
            "prio": {{
                "p_norm": {{"bg":0,"fg":0,"idle":0,"touch":0,"boost":0}}
            }},
            "rules": [
                {{"name":"self","regex":{},"pinned":true,
                  "rules":[{{"k":".","ac":"a_flip","pc":"p_norm"}}]}}
            ]
        }}"#, rx);
        TaskScheduler::new(serde_json::from_str(&json).unwrap())
    }

    fn self_mask() -> CpuSet {
        sched_getaffinity(Pid::from_raw(0)).unwrap()
    }

    #[test]
    fn scan_pins_self_threads_and_seeds() {
        let _lock = AFFINITY_LOCK.lock().unwrap();
        let mut sched = self_sched();
        let tid = unsafe { libc::gettid() } as u32;

        let seeds = sched.scan_and_apply_seeded();
        assert!(!seeds.is_empty(), "seed scan must find this process");
        assert!(seeds.iter().any(|(t, bits)| *t == tid && *bits == 0b1111),
                "main thread must be seeded with the little mask, got {:?}", seeds);

        let mask = self_mask();
        for cpu in 0..4 { assert!(mask.is_set(cpu).unwrap()); }
        for cpu in 4..8 { assert!(!mask.is_set(cpu).unwrap()); }
    }

    #[test]
    fn second_scan_keeps_pinning_and_uses_comm_cache() {
        let _lock = AFFINITY_LOCK.lock().unwrap();
        let mut sched = self_sched();
        sched.scan_and_apply_seeded();

        // eBPF mode on → comm cache active; a second non-force scan must
        // still re-pin (pinned rule) without touching /proc comm files.
        sched.set_ebpf_mode(true);
        sched.scan_and_apply();

        let mask = self_mask();
        for cpu in 0..4 { assert!(mask.is_set(cpu).unwrap()); }
        assert!(!sched.comm_cache.is_empty(), "comm cache must be populated");
        // Cached comm must be the real one for our main thread.
        let tid = unsafe { libc::gettid() } as u32;
        assert!(sched.comm_cache.get(&tid).is_some());
    }

    #[test]
    fn scene_change_reapplies_with_new_mask() {
        let _lock = AFFINITY_LOCK.lock().unwrap();
        let mut sched = scene_sched();
        // Default scene is Idle → "little" mask.
        sched.scan_and_apply_seeded();
        let mask = self_mask();
        for cpu in 0..4 { assert!(mask.is_set(cpu).unwrap()); }
        for cpu in 4..8 { assert!(!mask.is_set(cpu).unwrap()); }

        // Scene switch Idle→Touch flips the resolved mask to "big".
        // The read-check inside apply_action must NOT suppress this write:
        // current mask (little) differs from the target (big), so the
        // scheduler must re-pin — proving change detection doesn't
        // permanently freeze legitimate updates.
        sched.set_scene(SchedScene::Touch);
        sched.scan_and_apply();
        let mask = self_mask();
        for cpu in 4..8 { assert!(mask.is_set(cpu).unwrap()); }
        for cpu in 0..4 { assert!(!mask.is_set(cpu).unwrap()); }
    }

    #[test]
    fn unmatched_pids_land_in_negative_cache() {
        // A scheduler whose only rule matches nothing negative-caches every
        // visible pid after one scan, and keeps them for the next one.
        let mut none = TaskScheduler::new(serde_json::from_str(r#"{
            "enable": true, "cpumask": {}, "affinity": {}, "prio": {},
            "rules": [{"name":"x","regex":"^__never_matches__$",
                         "pinned":false,"rules":[]}]
        }"#).unwrap());
        none.scan_and_apply();
        let n1 = none.neg_cache.len();
        assert!(n1 > 0, "visible pids must be negative-cached");
        assert!(none.pid_cache.is_empty());
        none.scan_and_apply();
        assert!(none.neg_cache.len() >= n1.saturating_sub(5));
    }

    #[test]
    fn idle_scene_scans_slower_than_active() {
        let mut sched = self_sched();
        // Idle, clean: 1 s cadence.
        assert!(!sched.scan_due(Duration::from_millis(300)));
        assert!(sched.scan_due(Duration::from_millis(1000)));
        // Pending scene change: back to the 200 ms cadence.
        sched.set_scene(SchedScene::Touch);
        assert!(sched.scan_due(Duration::from_millis(200)));
        assert!(!sched.scan_due(Duration::from_millis(100)));
        // Active scene stays on 200 ms even after the dirty flag is consumed.
        sched.scene_dirty = false;
        assert!(sched.scan_due(Duration::from_millis(200)));
        // Back to Idle → dirty again (must re-apply idle masks promptly).
        sched.set_scene(SchedScene::Idle);
        assert!(sched.scan_due(Duration::from_millis(200)));
    }

    #[test]
    fn note_and_forget_comm() {
        let mut sched = self_sched();
        sched.set_ebpf_mode(true);
        let tid = unsafe { libc::gettid() } as u32;

        sched.note_comm(tid, "SelftestWorker");
        assert_eq!(sched.comm_cache.get(&tid).map(|s| s.as_str()), Some("SelftestWorker"));

        sched.forget_thread(tid);
        assert!(sched.comm_cache.get(&tid).is_none());
    }

    #[test]
    fn whitelist_prefixes_from_regexes() {
        let sched = self_sched();
        let p = sched.whitelist_prefixes();
        // Rule 1 is an escaped absolute path → long literal prefix; rule 2
        // is "." → no prefix. Both must be handled.
        assert!(p.iter().any(|s| s.contains("deps") || s.len() > 3));
        assert!(!p.iter().any(|s| s.is_empty()));
    }
}
