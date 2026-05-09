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
| Configs | Ships `mtd9400.json` and `mtd9400p.json` for Dimensity 9400/9400+ |

## Architecture

```
epoll loop
  ├── inotify fd  ──→  WatchEvent  (app switch / screen / mode file)
  ├── input fds   ──→  TouchEvent  (/dev/input/event*)
  └── timerfd     ──→  sample tick (40 ms)
           │
           ▼
      FSM (idle/touch/trigger/gesture/switch/junk)
           │
    ┌──────┼──────────┐
    ▼      ▼          ▼
 SysfsWriter  Governor  TaskScheduler
 (knobs)    (cpufreq)  (affinity/prio)
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
│   └── uperf                                   ← compiled binary
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
