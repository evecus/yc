// watcher/mod.rs
// inotify-based watchers:
//   1. Cpuset top-app cgroup.procs  → foreground app change (AppSwitch)
//   2. /sys/power/wake_lock         → screen on/off
//   3. cur_powermode.txt            → user mode switch

use std::fs;
use std::os::unix::io::AsRawFd;
use inotify::{Inotify, WatchMask};
use anyhow::{Result, Context};

pub enum WatchEvent {
    AppSwitch,
    ScreenOn,
    ScreenOff,
    ModeChange(String),
}

pub struct Watcher {
    inotify:       Inotify,
    wd_cpuset:     Option<inotify::WatchDescriptor>,
    wd_wakelock:   Option<inotify::WatchDescriptor>,
    wd_switchfile: Option<inotify::WatchDescriptor>,
    switch_path:   String,
    screen_on:     bool,
}

impl Watcher {
    pub fn new(switch_inode: &str) -> Result<Self> {
        let mut inotify = Inotify::init()
            .context("inotify init failed")?;

        let mask = WatchMask::MODIFY | WatchMask::CLOSE_WRITE;

        // cpuset top-app (cgroup v1 and v2 paths)
        let wd_cpuset = inotify.add_watch("/dev/cpuset/top-app/cgroup.procs", mask)
            .or_else(|_| inotify.add_watch("/sys/fs/cgroup/top-app/cgroup.procs", mask))
            .ok();

        // wake_lock for screen state
        let wd_wakelock = inotify.add_watch("/sys/power/wake_lock", mask).ok();

        // user mode-switch file
        let wd_switchfile = inotify.add_watch(switch_inode, mask).ok();

        if wd_cpuset.is_none()     { log::warn!("watcher: cpuset procs not watchable"); }
        if wd_wakelock.is_none()   { log::warn!("watcher: wake_lock not watchable"); }
        if wd_switchfile.is_none() { log::warn!("watcher: switch inode {} not watchable", switch_inode); }

        Ok(Self {
            inotify,
            wd_cpuset,
            wd_wakelock,
            wd_switchfile,
            switch_path: switch_inode.to_string(),
            screen_on: true,
        })
    }

    pub fn fd(&self) -> std::os::unix::io::RawFd {
        self.inotify.as_raw_fd()
    }

    /// Drain all pending inotify events → high-level WatchEvents.
    pub fn drain(&mut self) -> Vec<WatchEvent> {
        let mut buf = [0u8; 4096];
        let mut out = Vec::new();

        match self.inotify.read_events(&mut buf) {
            Ok(events) => {
                for event in events {
                    if self.wd_cpuset.as_ref().map(|w| *w == event.wd).unwrap_or(false) {
                        out.push(WatchEvent::AppSwitch);
                    } else if self.wd_wakelock.as_ref().map(|w| *w == event.wd).unwrap_or(false) {
                        let new_on = is_screen_on();
                        if new_on != self.screen_on {
                            self.screen_on = new_on;
                            out.push(if new_on { WatchEvent::ScreenOn }
                                     else      { WatchEvent::ScreenOff });
                        }
                    } else if self.wd_switchfile.as_ref().map(|w| *w == event.wd).unwrap_or(false) {
                        let mode = fs::read_to_string(&self.switch_path)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !mode.is_empty() {
                            out.push(WatchEvent::ModeChange(mode));
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => log::warn!("watcher: inotify read error: {}", e),
        }
        out
    }
}

fn is_screen_on() -> bool {
    fs::read_to_string("/sys/power/wake_lock")
        .map(|s| s.contains("PowerManagerService.Display"))
        .unwrap_or(true)
}
