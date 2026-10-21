// uperf-ebpf — kernel side of uperf's event-driven TaskScheduler.
//
// Hooks four scheduler tracepoints and reports process/thread lifecycle
// events to userspace through a RingBuf:
//
//   sched_process_fork   → new thread inside an already-tracked process
//   sched_process_exec   → process image replaced (native binaries)
//   task_rename          → comm changed (Android app processes get renamed
//                          from the zygote name to the package name here)
//   sched_process_exit   → thread gone, drop bookkeeping
//
// The kernel side never touches CPU affinity itself — it only *filters*
// (whitelist + tracked-parent propagation) and *reports*. Userspace
// resolves the uperf rules (regex on cmdline / thread comm, scene-aware)
// and performs the actual sched_setaffinity / setpriority /
// sched_setscheduler calls. This keeps all policy in userspace where the
// JSON config lives, and the kernel side stays tiny and portable.
//
// Tracepoint field offsets (child_pid / child_comm / newcomm) differ
// across kernel versions, so they are parsed from tracefs `format`
// files at load time and injected via OFFSETS_MAP (index 0).

#![no_std]
#![no_main]
#![allow(linker_messages)]
#![allow(dead_code)]

use aya_ebpf::{
    helpers::{bpf_get_current_comm, bpf_get_current_pid_tgid},
    macros::{map, tracepoint},
    maps::{Array, HashMap, LruHashMap, RingBuf},
    programs::TracePointContext,
};

const EVENT_FORK: u32 = 1;
const EVENT_EXEC: u32 = 2;
const EVENT_RENAME: u32 = 3;
const EVENT_EXIT: u32 = 4;

/// Layout must match userspace `TracepointOffsets`.
/// fork_child_pid == 0 means "not injected yet" (see offsets_load).
#[repr(C)]
#[derive(Clone, Copy)]
struct TracepointOffsets {
    fork_child_pid: u32,
    fork_child_comm: u32,
    rename_newcomm: u32,
}

/// Process event — layout must match userspace `EbpfProcEvent`.
#[repr(C)]
struct ProcEvent {
    pid: i32,
    tid: i32,
    comm: [u8; 16],
    event_type: u32,
}

const MAP_CAPACITY: u32 = 16 << 5;
const APPLIED_CAPACITY: u32 = 8192;

/// Whitelist: 15-byte keys derived from the literal prefixes of uperf's
/// process regexes (e.g. rule "com\.tencent\.mm" → key "com.tencent.mm").
/// comm is 15 usable chars (TASK_COMM_LEN=16 incl. NUL), so keys are 15B.
#[map]
static TARGET_COMM_MAP: HashMap<[u8; 15], u32> = HashMap::with_max_entries(MAP_CAPACITY, 0);

/// tid → affinity bits of tracked threads. Presence means "tracked";
/// children of tracked threads inherit event reporting (LruHashMap so a
/// full map degrades gracefully instead of blocking inserts).
#[map]
static APPLIED_MAP: LruHashMap<u32, u64> = LruHashMap::with_max_entries(APPLIED_CAPACITY, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Userspace-injected tracepoint field offsets, single entry at index 0.
#[map]
static OFFSETS_MAP: Array<TracepointOffsets> = Array::with_max_entries(1, 0);

/// Load offsets; fork_child_pid == 0 means "not injected".
#[inline(always)]
fn offsets_load() -> Option<TracepointOffsets> {
    OFFSETS_MAP.get(0).and_then(|o| {
        if o.fork_child_pid != 0 {
            Some(*o)
        } else {
            None
        }
    })
}

/// Event metadata: pid = tgid (process), tid = thread id.
struct EventMeta {
    pid: i32,
    tid: i32,
    comm: [u8; 16],
    parent_tid: u32,
    parent_tgid: u32,
}

#[inline(always)]
fn applied_lookup(key: u32) -> bool {
    unsafe { APPLIED_MAP.get(key) }.is_some()
}

/// Slide a 15-byte window over the 16-byte comm and probe the whitelist.
/// Position 0 covers prefix keys; position 1 covers suffix-style keys.
#[inline(always)]
fn whitelist_matched(comm: &[u8; 16]) -> bool {
    for pos in 0..=1usize {
        let key: [u8; 15] = comm[pos..pos + 15].try_into().unwrap_or([0u8; 15]);
        if unsafe { TARGET_COMM_MAP.get(key) }.is_some() {
            return true;
        }
    }
    false
}

#[inline(always)]
fn submit_event(event: ProcEvent) {
    if let Some(mut entry) = EVENTS.reserve(0) {
        entry.write(event);
        entry.submit(0);
    }
}

/// Parse fork tracepoint fields. pid is the parent's tgid so that clone
/// children sharing the tgid are reported against the right process;
/// tid is the freshly created child.
#[inline(always)]
fn fork_parse(ctx: &TracePointContext, offsets: &TracepointOffsets) -> EventMeta {
    let pid_tgid = bpf_get_current_pid_tgid();
    let parent_tid = (pid_tgid & 0xFFFFFFFF) as u32;
    let parent_tgid = (pid_tgid >> 32) as u32;
    let child_pid = unsafe { ctx.read_at::<i32>(offsets.fork_child_pid as usize).unwrap_or(0) };
    let child_comm =
        unsafe { ctx.read_at::<[u8; 16]>(offsets.fork_child_comm as usize).unwrap_or([0u8; 16]) };
    EventMeta {
        pid: parent_tgid as i32,
        tid: child_pid,
        comm: child_comm,
        parent_tid,
        parent_tgid,
    }
}

#[inline(always)]
fn exec_parse() -> EventMeta {
    let pid_tgid = bpf_get_current_pid_tgid();
    EventMeta {
        pid: (pid_tgid >> 32) as i32,
        tid: pid_tgid as i32,
        comm: bpf_get_current_comm().unwrap_or_default(),
        parent_tid: 0,
        parent_tgid: 0,
    }
}

/// FORK checks the parent (tracked thread propagation), EXEC checks self.
/// Whitelist is the fallback for both. FORK of a tracked thread reserves
/// APPLIED_MAP[child]=0 immediately to avoid a rename race.
#[inline(always)]
fn report_named_event(ctx: &TracePointContext, event_type: u32) -> u32 {
    let meta = if event_type == EVENT_FORK {
        let Some(offsets) = offsets_load() else {
            return 0;
        };
        fork_parse(ctx, &offsets)
    } else {
        exec_parse()
    };

    let is_tracked = if event_type == EVENT_FORK {
        applied_lookup(meta.parent_tid)
    } else {
        applied_lookup(meta.tid as u32)
    };

    if !is_tracked && !whitelist_matched(&meta.comm) {
        return 0;
    }

    if event_type == EVENT_FORK && is_tracked {
        let _ = APPLIED_MAP.insert(meta.tid as u32, 0, 0);
    }

    submit_event(ProcEvent {
        pid: meta.pid,
        tid: meta.tid,
        comm: meta.comm,
        event_type,
    });
    0
}

#[tracepoint(name = "sched_process_fork", category = "sched")]
fn sched_process_fork(ctx: TracePointContext) -> u32 {
    report_named_event(&ctx, EVENT_FORK)
}

#[tracepoint(name = "sched_process_exec", category = "sched")]
fn sched_process_exec(ctx: TracePointContext) -> u32 {
    report_named_event(&ctx, EVENT_EXEC)
}

/// Catch thread renames (Android app processes are renamed from the
/// zygote name to the package name — this is the main hook for apps).
/// Tracked threads pass unconditionally; others need the whitelist.
#[tracepoint(name = "task_rename", category = "task")]
fn task_rename(ctx: TracePointContext) -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;
    if tid == 0 {
        return 0;
    }
    let tgid = (pid_tgid >> 32) as u32;

    let tracked = applied_lookup(tid);

    let Some(offsets) = offsets_load() else {
        return 0;
    };
    let new_comm =
        unsafe { ctx.read_at::<[u8; 16]>(offsets.rename_newcomm as usize).unwrap_or([0u8; 16]) };

    if !tracked && !whitelist_matched(&new_comm) {
        return 0;
    }

    submit_event(ProcEvent {
        pid: tgid as i32,
        tid: tid as i32,
        comm: new_comm,
        event_type: EVENT_RENAME,
    });
    0
}

/// Thread exit: drop bookkeeping and let userspace clean its caches.
#[tracepoint(name = "sched_process_exit", category = "sched")]
fn sched_process_exit(_ctx: TracePointContext) -> u32 {
    let pid_tgid = bpf_get_current_pid_tgid();
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;
    let tgid = (pid_tgid >> 32) as u32;

    if !applied_lookup(tid) {
        return 0;
    }

    let _ = APPLIED_MAP.remove(tid);

    submit_event(ProcEvent {
        pid: tgid as i32,
        tid: tid as i32,
        comm: [0u8; 16],
        event_type: EVENT_EXIT,
    });
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
