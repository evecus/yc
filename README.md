# uperf-rs

Rust rewrite of [yc9559/uperf](https://github.com/yc9559/uperf) — a userspace
performance controller for Android.  Drop-in compatible with the original JSON
config format.

## What's new vs the original

| Area | Change |
|------|--------|
| Runtime | Rewritten in Rust; no closed binary |
| Android support | Tested path for Android 13–15 (cgroup v2 fallback) |
| Root frameworks | KernelSU / APatch compatible (no Magisk hard-dependency) |
| SfAnalysis | Removed — field kept in config for compatibility but ignored |
| Configs | Dimensity 8300/9400 (`mtd*.json`) and Snapdragon 8+ Gen 1 / 8 Gen 2 / 8 Gen 3 (`sd8gen1p.json`, `sd8gen2.json`, `sd8gen3.json`), plus `auto` runtime detection for unknown SoCs |
| TaskScheduler | **eBPF event-driven mode** (optional, see below) — fork/exec/rename/exit tracepoints replace the 200 ms /proc poll for new-thread latency |
| Platform coverage | 8 additional SoC configs (Dimensity 9000/9200/9300/9500, Snapdragon 8 Elite / 8 Elite Gen 2 / 8s Gen 3 / 8s Gen 4); unknown platforms auto-detect topology at boot |
| util sampling | Foreground cgroup accounting instead of global `/proc/stat` (well-known v1/v2 paths, then a generic scan of every cgroup mount in `/proc/mounts` for OEM-remounted hierarchies, then `/proc/stat` fallback) |
| Frequency control | Segmented power model (free/sweet/plain/turbo) + write hysteresis |
| Scheduler overhead | Lazy cmdline reads (pid-cache hits skip /proc), read-before-write action application (affinity/nice/RT skipped when already in the target state), and an eBPF-mode comm cache fed by rename/exit events — the 200 ms fallback poll no longer costs ~20% of a core on MTK procfs |
| Watcher overhead | Rate-limited `/sys/power/wake_lock` re-reads (≤4/s) and coalesced AppSwitch events (≤7/s) — the file is one of the chattiest on Android and used to be re-read per inotify event |
| Observability | Every 5 s a `perf:` line with per-thread CPU of the daemon plus scan/governor/ebpf-event/loop-rate breakdown, so idle-CPU regressions are attributable without strace |

## eBPF mode (optional)

When the kernel has BTF (`/sys/kernel/btf/vmlinux`) and the module ships
`bin/uperf-ebpf`, uperf attaches four scheduler tracepoints
(`sched_process_fork/exec/exit`, `task_rename`) and applies thread rules
within microseconds of a thread appearing. All policy stays in userspace;
the kernel side only filters (whitelist + tracked-parent propagation) and
reports events over a RingBuf.

Any failure — no BTF, missing object, attach error — silently falls back
to the classic 200 ms /proc scan. Force-disable with `UPERF_NO_EBPF=1`.

The fallback scan itself is cheap: cmdline is only read for pids not yet
in the pid→rule cache, writes are skipped when the current affinity/nice/RT
state already matches the target, and in eBPF mode thread names come from
the rename/exit event stream instead of per-thread /proc reads.

Building the kernel object (needs nightly + bpf-linker):

```bash
./build-ebpf.sh     # builds uperf-ebpf and copies it into module/bin/
```

See `build-ebpf.sh` header comments for toolchain setup notes.

## Architecture

```
epoll loop
  ├── inotify fd  ──→  WatchEvent  (app switch / screen / mode file)
  ├── input fds   ──→  TouchEvent  (/dev/input/event*)
  ├── timerfd     ──→  sample tick (40 ms)
  └── ebpf ringbuf ─→ ProcEvent   (fork/exec/rename/exit, optional)
           │
           ▼
      FSM (idle/touch/trigger/gesture/switch/junk)
           │
    ┌──────┼──────────┐
    ▼      ▼          ▼
 SysfsWriter  Governor  TaskScheduler
 (knobs)    (cpufreq)  (affinity/prio, eBPF-accelerated)
```

## Cross-compiling for aarch64 Android

```bash
# Install Android NDK (r26 or later)
export NDK=$HOME/android-ndk-r26d
export TOOLCHAIN=$NDK/toolchains/llvm/prebuilt/linux-x86_64

# Add Rust target
rustup target add aarch64-linux-android

# Configure cargo linker
cat >> ~/.cargo/config.toml << 'EOF'
[target.aarch64-linux-android]
linker = "aarch64-linux-android34-clang"
EOF

export PATH="$TOOLCHAIN/bin:$PATH"
cargo build --release --target aarch64-linux-android
```

Output: `target/aarch64-linux-android/release/uperf`

## Magisk / KernelSU module layout

```
module/
├── META-INF/com/google/android/update-binary   ← install script
├── META-INF/com/google/android/updater-script
├── module.prop
├── bin/
│   ├── uperf                                   ← compiled binary
│   └── uperf-ebpf                              ← eBPF object (optional; see "eBPF mode")
├── config/
│   ├── mtd9400.json
│   └── mtd9400p.json
└── service.sh                                  ← starts uperf on boot
```

## Config format

Identical to the original uperf JSON.  The `sfanalysis.enable` field is
accepted but always treated as `false`.

### Dimensity 9400 cluster layout

| Cluster | Cores    | CPU IDs | Max freq |
|---------|----------|---------|----------|
| c0      | 3× A520  | 0–2     | 2.0 GHz  |
| c1      | 4× A725  | 3–6     | 3.33 GHz |
| c2      | 1× X925  | 7       | 3.63 GHz |

### Dimensity 9400+ differences

| Cluster | Max freq |
|---------|----------|
| c1      | 3.4 GHz  |
| c2      | 3.73 GHz |

### Snapdragon cluster layout

| Config | Platform (`ro.board.platform`) | Clusters (cpu ids) | GPU |
|--------|-------------------------------|--------------------|-----|
| `sd8gen1p` | `taro` (8+ Gen 1, also used for 8 Gen 1) | c0 4×A510 (0–3), c1 3×A710 (4–6), c2 1×X2 (7) | Adreno 730 |
| `sd8gen2`  | `kalama`    | c0 3×A510 (0–2), c1 2×A715 (3–4), c2 2×A710 (5–6), c3 1×X3 (7) | Adreno 740 |
| `sd8gen3`  | `pineapple` | c0 2×A520 (0–1), c1 3×A720 (2–4), c2 2×A720 (5–6), c3 1×X4 (7) | Adreno 750 |

The GPU controller drives `/sys/class/kgsl/kgsl-3d0/devfreq/{min,max}_freq`.
Verify frequency tables on your device with
`cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_available_frequencies`.

## Performance modes

| Mode | Description |
|------|-------------|
| `powersave`   | Locks clocks low; forces LITTLE-only cpuset at idle |
| `balance`     | Default: responsive but power-aware |
| `performance` | Allows full turbo on trigger/switch hints |
| `fast`        | Max clocks pinned, minimal limiting |

Write the mode name to the `switchInode` path (default
`/sdcard/Android/yc/uperf/cur_powermode.txt`) at runtime.

## License

Apache 2.0 — same as original uperf.
