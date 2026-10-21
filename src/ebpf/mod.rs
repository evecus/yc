// ebpf/mod.rs
// eBPF event-driven acceleration for the TaskScheduler.
//
// The kernel side (`uperf-ebpf` crate, loaded as an ELF shipped next to
// the uperf binary) hooks sched_process_fork / sched_process_exec /
// task_rename / sched_process_exit and reports matching thread events
// through a RingBuf. This module owns that kernel state:
//
//   • loads + attaches the programs (BTF-gated, tracefs offsets injected)
//   • maintains the in-kernel whitelist (TARGET_COMM_MAP) from the
//     literal prefixes of the configured process regexes
//   • drains events and hands them to TaskScheduler::apply_thread,
//     which performs the actual affinity / nice / SCHED_FIFO syscalls
//   • mirrors applied threads into APPLIED_MAP so the kernel reports
//     their future children without a /proc scan
//
// Every failure path (no BTF, missing ELF, load/attach error, tracefs
// unavailable) degrades to None and the caller keeps the classic 200 ms
// /proc scan — zero behavior change on unsupported devices.
//
// Set UPERF_NO_EBPF=1 to force-disable for debugging.

use std::fs;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc;
use std::thread;

use aya::maps::{Array as AyaArray, HashMap as AyaHashMap};
use aya::{programs::TracePoint as AyaTracePoint, Ebpf, EbpfLoader, Pod};

use crate::sched::TaskScheduler;

/// Kernel-side ProcEvent — layout must match uperf-ebpf's ProcEvent.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EbpfProcEvent {
    pub pid: i32,
    pub tid: i32,
    pub comm: [u8; 16],
    pub event_type: u32,
}

pub const EBPF_EVENT_FORK: u32   = 1;
pub const EBPF_EVENT_EXEC: u32   = 2;
pub const EBPF_EVENT_RENAME: u32 = 3;
pub const EBPF_EVENT_EXIT: u32   = 4;

/// Kernel-side TracepointOffsets — layout must match uperf-ebpf.
#[repr(C)]
#[derive(Clone, Copy)]
struct EbpfOffsets {
    fork_child_pid: u32,
    fork_child_comm: u32,
    rename_newcomm: u32,
}

unsafe impl Pod for EbpfOffsets {}

/// Trim a kernel comm (16 bytes, NUL-padded) to &str.
fn comm_str(comm: &[u8; 16]) -> &str {
    let end = comm.iter().position(|&b| b == 0).unwrap_or(16);
    std::str::from_utf8(&comm[..end]).unwrap_or("").trim()
}

/// Truncate a string to at most 15 bytes on a char boundary
/// (TASK_COMM_LEN - 1 usable chars in comm).
fn key15(s: &str) -> [u8; 15] {
    let bytes = s.as_bytes();
    let end = if bytes.len() <= 15 { bytes.len() } else {
        // back off to a char boundary
        let mut e = 15;
        while e > 0 && !s.is_char_boundary(e) { e -= 1; }
        e
    };
    let mut key = [0u8; 15];
    key[..end].copy_from_slice(&bytes[..end]);
    key
}

pub struct EbpfState {
    event_rx:      mpsc::Receiver<EbpfProcEvent>,
    reader_thread: Option<thread::JoinHandle<()>>,
    bpf:           Ebpf,
    wakeup_fd:     RawFd,
    comm_capacity: u32,
    /// Events dispatched since init (for the log line).
    pub events:    u64,
}

impl Drop for EbpfState {
    fn drop(&mut self) {
        // Wake the reader thread's epoll_wait via eventfd, then join.
        if self.wakeup_fd >= 0 {
            let val: u64 = 1;
            unsafe {
                libc::write(self.wakeup_fd, &val as *const u64 as *const _, 8);
            }
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if self.wakeup_fd >= 0 {
            unsafe { libc::close(self.wakeup_fd); }
        }
    }
}

/// Kernel eBPF support probe: BTF is required by aya's loader.
pub fn probe() -> bool {
    fs::metadata("/sys/kernel/btf/vmlinux").is_ok()
}

fn attach_tracepoint(bpf: &mut Ebpf, category: &str, name: &str, required: bool) -> bool {
    let Some(prog) = bpf.program_mut(name) else {
        if required { log::warn!("ebpf: program {name} not found in object"); }
        return false;
    };
    let Ok(tp): Result<&mut AyaTracePoint, _> = prog.try_into() else {
        if required { log::warn!("ebpf: {name} is not a tracepoint program"); }
        return false;
    };
    if let Err(e) = tp.load() {
        if required { log::warn!("ebpf: {name} load failed: {e}"); }
        return false;
    }
    if let Err(e) = tp.attach(category, name) {
        if required { log::warn!("ebpf: {name} attach failed: {e}"); }
        return false;
    }
    true
}

/// (Re)build TARGET_COMM_MAP from the scheduler's literal prefixes.
/// Returns true when the map is too small and a full reload is needed.
fn comm_map_init(bpf: &mut Ebpf, prefixes: &[String], capacity: u32) -> bool {
    let mut entries: Vec<[u8; 15]> = prefixes.iter().map(|p| key15(p)).collect();
    entries.sort();
    entries.dedup();

    if entries.len() > capacity as usize {
        log::warn!(
            "ebpf: whitelist needs {} entries > map capacity {capacity} — reload",
            entries.len()
        );
        return true;
    }

    let Some(map) = bpf.map_mut("TARGET_COMM_MAP") else {
        log::warn!("ebpf: TARGET_COMM_MAP not found in object");
        return false;
    };
    let mut target_map = match AyaHashMap::<_, [u8; 15], u32>::try_from(map) {
        Ok(m)  => m,
        Err(e) => { log::warn!("ebpf: TARGET_COMM_MAP type mismatch: {e}"); return false; }
    };

    let old_keys: Vec<[u8; 15]> = target_map.keys().filter_map(|r| r.ok()).collect();
    for key in &old_keys { let _ = target_map.remove(key); }

    let mut count = 0;
    for key in &entries {
        if target_map.insert(key, 1, 0).is_ok() { count += 1; }
    }

    if count < entries.len() {
        log::warn!("ebpf: whitelist partially inserted ({count}/{}) — reload", entries.len());
        return true;
    }

    log::info!("ebpf: whitelist armed — {} prefix keys", count);
    false
}

/// Locate tracefs (modern kernels expose /sys/kernel/tracing).
fn tracefs_root() -> Option<&'static str> {
    if fs::metadata("/sys/kernel/tracing").is_ok() {
        return Some("/sys/kernel/tracing");
    }
    if fs::metadata("/sys/kernel/debug/tracing").is_ok() {
        return Some("/sys/kernel/debug/tracing");
    }
    None
}

/// Parse a tracepoint `format` file into field-name → offset.
fn tracepoint_parse(root: &str, category: &str, name: &str)
    -> Option<std::collections::HashMap<String, u32>>
{
    let path = format!("{root}/events/{category}/{name}/format");
    let content = fs::read_to_string(&path).ok()?;
    let mut offsets = std::collections::HashMap::new();
    for line in content.lines() {
        let Some(rest) = line.trim().strip_prefix("field:") else { continue };
        let parts: Vec<&str> = rest.split(';').map(|s| s.trim()).collect();
        if parts.len() < 2 { continue; }
        let field = parts[0].split_whitespace().last().unwrap_or("")
            .split('[').next().unwrap_or("");
        if field.is_empty() { continue; }
        for part in &parts[1..] {
            if let Some(off) = part.strip_prefix("offset:")
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                offsets.insert(field.to_string(), off);
                break;
            }
        }
    }
    Some(offsets)
}

/// Inject this machine's tracepoint field offsets into OFFSETS_MAP[0].
/// Offsets differ across kernel versions — never hardcode them.
fn offsets_inject(bpf: &mut Ebpf) -> bool {
    let Some(root) = tracefs_root() else {
        log::warn!("ebpf: tracefs unavailable — falling back to /proc scan");
        return false;
    };

    let offsets = (|| {
        let fork   = tracepoint_parse(root, "sched", "sched_process_fork")?;
        let rename = tracepoint_parse(root, "task",  "task_rename")?;
        Some(EbpfOffsets {
            fork_child_pid:  *fork.get("child_pid")?,
            fork_child_comm: *fork.get("child_comm")?,
            rename_newcomm:  *rename.get("newcomm")?,
        })
    })();

    let Some(offsets) = offsets else {
        log::warn!("ebpf: tracepoint format parse failed [{root}] — falling back to /proc scan");
        return false;
    };

    let Some(map) = bpf.map_mut("OFFSETS_MAP") else {
        log::warn!("ebpf: OFFSETS_MAP not found — falling back to /proc scan");
        return false;
    };
    let Ok(mut offsets_map) = AyaArray::<_, EbpfOffsets>::try_from(map) else {
        log::warn!("ebpf: OFFSETS_MAP type mismatch — falling back to /proc scan");
        return false;
    };
    if offsets_map.set(0, offsets, 0).is_err() {
        log::warn!("ebpf: offset injection failed — falling back to /proc scan");
        return false;
    }

    log::info!(
        "ebpf: tracepoint offsets [{root}] fork[child_pid={}, child_comm={}] rename[newcomm={}]",
        offsets.fork_child_pid, offsets.fork_child_comm, offsets.rename_newcomm
    );
    true
}

/// Load the eBPF object shipped next to the uperf binary, attach the
/// four tracepoints, arm the whitelist, and spawn the RingBuf reader.
/// Returns None on any failure — caller keeps the /proc scan.
pub fn init(sched: &TaskScheduler) -> Option<EbpfState> {
    if std::env::var("UPERF_NO_EBPF").is_ok_and(|v| v == "1") {
        log::info!("ebpf: disabled via UPERF_NO_EBPF=1 — using /proc scan");
        return None;
    }
    if !probe() {
        log::info!("ebpf: kernel has no BTF — using /proc scan");
        return None;
    }

    let prefixes = sched.whitelist_prefixes();
    if prefixes.is_empty() {
        log::info!("ebpf: no literal rule prefixes to whitelist — using /proc scan");
        return None;
    }

    let ebpf_path = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.join("uperf-ebpf")))?;
    if !ebpf_path.exists() {
        log::info!("ebpf: {} not shipped — using /proc scan", ebpf_path.display());
        return None;
    }
    let ebpf_data = match fs::read(&ebpf_path) {
        Ok(d) if !d.is_empty() => d,
        Ok(_)  => { log::warn!("ebpf: {} is empty", ebpf_path.display()); return None; }
        Err(e) => { log::warn!("ebpf: cannot read {}: {e}", ebpf_path.display()); return None; }
    };

    let capacity = (prefixes.len().max(512)).next_power_of_two() as u32;

    let mut loader = EbpfLoader::new();
    loader.map_max_entries("TARGET_COMM_MAP", capacity);
    let mut bpf = match loader.load(&ebpf_data) {
        Ok(b)  => b,
        Err(e) => { log::warn!("ebpf: load failed: {e} — using /proc scan"); return None; }
    };

    // Inject offsets before attaching so the first events parse fields.
    if !offsets_inject(&mut bpf) { return None; }

    if !attach_tracepoint(&mut bpf, "sched", "sched_process_fork", true) { return None; }
    if !attach_tracepoint(&mut bpf, "sched", "sched_process_exec", true) { return None; }
    if !attach_tracepoint(&mut bpf, "sched", "sched_process_exit", true) { return None; }
    if !attach_tracepoint(&mut bpf, "task",  "task_rename",        true) { return None; }

    if comm_map_init(&mut bpf, &prefixes, capacity) {
        // Capacity math above guarantees this shouldn't happen.
        log::warn!("ebpf: whitelist capacity exceeded — using /proc scan");
        return None;
    }

    let ring_buf = match bpf.take_map("EVENTS") {
        Some(map) => match aya::maps::RingBuf::try_from(map) {
            Ok(rb)  => rb,
            Err(e) => { log::warn!("ebpf: EVENTS map type mismatch: {e}"); return None; }
        },
        None => { log::warn!("ebpf: EVENTS map not found"); return None; }
    };

    let (tx, rx) = mpsc::channel::<EbpfProcEvent>();
    let wakeup_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wakeup_fd < 0 {
        log::warn!("ebpf: eventfd failed: {}", std::io::Error::last_os_error());
        return None;
    }
    let reader_thread = thread::Builder::new()
        .name("EbpfReader".into())
        .spawn(move || ebpf_reader(ring_buf, tx, wakeup_fd))
        .ok()?;

    log::info!("ebpf: event-driven scheduler online ({} whitelist prefixes)", prefixes.len());

    Some(EbpfState {
        event_rx: rx,
        reader_thread: Some(reader_thread),
        bpf,
        wakeup_fd,
        comm_capacity: capacity,
        events: 0,
    })
}

/// RingBuf reader thread — epoll on the ringbuf fd plus a wakeup eventfd
/// used by Drop to shut the thread down cleanly.
fn ebpf_reader(
    mut ring_buf: aya::maps::RingBuf<aya::maps::MapData>,
    tx: mpsc::Sender<EbpfProcEvent>,
    wakeup_fd: RawFd,
) {
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 { log::warn!("ebpf: epoll_create1 failed"); return; }

    let ring_fd = ring_buf.as_raw_fd();
    let mut ring_ev: libc::epoll_event = unsafe { std::mem::zeroed() };
    ring_ev.events = libc::EPOLLIN as u32;
    ring_ev.u64 = 0;
    if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, ring_fd, &mut ring_ev) } < 0 {
        log::warn!("ebpf: epoll_ctl(ring) failed");
        unsafe { libc::close(epfd); }
        return;
    }

    let mut wake_ev: libc::epoll_event = unsafe { std::mem::zeroed() };
    wake_ev.events = libc::EPOLLIN as u32;
    wake_ev.u64 = 1;
    if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, wakeup_fd, &mut wake_ev) } < 0 {
        log::warn!("ebpf: epoll_ctl(wakeup) failed");
        unsafe { libc::close(epfd); }
        return;
    }

    let mut events: [libc::epoll_event; 2] = unsafe { std::mem::zeroed() };
    loop {
        let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), 2, -1) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        // Wakeup event takes priority — shut down.
        if events.iter().take(n as usize).any(|e| e.u64 == 1) { break; }

        while let Some(item) = ring_buf.next() {
            let bytes: &[u8] = &item;
            if bytes.len() >= std::mem::size_of::<EbpfProcEvent>() {
                let event: EbpfProcEvent =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const EbpfProcEvent) };
                if tx.send(event).is_err() {
                    unsafe { libc::close(epfd); }
                    return;
                }
            }
        }
    }
    unsafe { libc::close(epfd); }
}

// ── APPLIED_MAP accessors ─────────────────────────────────────────────────────

fn applied_map(bpf: &mut Ebpf) -> Option<AyaHashMap<&mut aya::maps::MapData, u32, u64>> {
    AyaHashMap::<_, u32, u64>::try_from(bpf.map_mut("APPLIED_MAP")?).ok()
}

fn applied_set(bpf: &mut Ebpf, tid: i32, bits: u64) {
    let Some(mut m) = applied_map(bpf) else { return };
    if let Err(e) = m.insert(tid as u32, bits, 0) {
        log::trace!("ebpf: APPLIED_MAP insert tid={tid} failed: {e}");
    }
}

fn applied_del(bpf: &mut Ebpf, tid: i32) {
    let Some(mut m) = applied_map(bpf) else { return };
    let _ = m.remove(&(tid as u32));
}

fn applied_clear(bpf: &mut Ebpf) {
    let Some(mut m) = applied_map(bpf) else { return };
    let keys: Vec<u32> = m.keys().filter_map(|r| r.ok()).collect();
    for k in &keys { let _ = m.remove(k); }
}

// ── Event dispatch ────────────────────────────────────────────────────────────

impl EbpfState {
    /// Dispatch all pending kernel events to the TaskScheduler.
    /// FORK/EXEC/RENAME → apply_thread; EXIT → untrack.
    pub fn drain(&mut self, sched: &mut TaskScheduler) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.events += 1;
            let tid  = event.tid;
            let pid  = event.pid;
            let comm = comm_str(&event.comm);

            match event.event_type {
                EBPF_EVENT_EXIT => {
                    applied_del(&mut self.bpf, tid);
                    sched.forget_thread(tid as u32);
                }
                EBPF_EVENT_EXEC | EBPF_EVENT_RENAME => {
                    // Kernel already whitelist/tracked this thread; userspace
                    // resolves the actual rule (cmdline regex) and applies.
                    // Refresh the comm cache first so the periodic scan sees
                    // the new name without re-opening /proc/{tid}/comm.
                    sched.note_comm(tid as u32, comm);
                    match sched.apply_thread(pid as u32, tid as u32, comm) {
                        Some((_, bits)) => applied_set(&mut self.bpf, tid, bits),
                        None           => applied_del(&mut self.bpf, tid),
                    }
                }
                EBPF_EVENT_FORK => {
                    // Child of a tracked thread: kernel reserved the entry.
                    // Wait for its RENAME/EXEC to resolve the real rule —
                    // at fork time the child's comm is still the parent's.
                }
                _ => {}
            }
        }
    }

    /// Re-arm the whitelist after a config reload. Returns true when a
    /// full eBPF reload is required (caller should re-init).
    /// (uperf currently loads its config once at startup — this is here
    /// for when hot-reload lands.)
    #[allow(dead_code)]
    pub fn refresh_whitelist(&mut self, sched: &TaskScheduler) -> bool {
        let prefixes = sched.whitelist_prefixes();
        comm_map_init(&mut self.bpf, &prefixes, self.comm_capacity)
    }

    /// One full /proc scan (force semantics) whose results seed
    /// APPLIED_MAP, so children of currently-tracked threads are
    /// reported by the kernel from now on. Called once after init.
    pub fn seed_from_scan(&mut self, sched: &mut TaskScheduler) {
        applied_clear(&mut self.bpf);
        let seeds = sched.scan_and_apply_seeded();
        for (tid, bits) in &seeds {
            applied_set(&mut self.bpf, *tid as i32, *bits);
        }
        log::info!("ebpf: seeded {} tracked threads from /proc scan", seeds.len());
    }
}