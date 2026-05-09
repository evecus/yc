// input/mod.rs
// Reads raw Linux input events from /dev/input/event* to detect:
//   - Touch down / up
//   - Swipe (ABS_X or ABS_Y displacement > threshold)
//   - Full-screen edge gesture (starting near screen border)
//
// This is the same approach as the original uperf: open all
// touchscreen event nodes and poll them with epoll.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use anyhow::{Result, Context};
use crate::config::InputModule;

// Linux input_event structure (from linux/input.h):
//   struct input_event {
//       struct timeval time;   // 8 or 16 bytes depending on arch
//       __u16 type;
//       __u16 code;
//       __s32 value;
//   };
// On aarch64 timeval is two i64 fields = 16 bytes.
const EV_ABS: u16 = 3;
const EV_SYN: u16 = 0;
const ABS_MT_TRACKING_ID: u16 = 57;
const ABS_MT_POSITION_X: u16  = 53;
const ABS_MT_POSITION_Y: u16  = 54;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const SYN_REPORT: u16 = 0;

/// Events emitted by the InputReader
#[derive(Debug, Clone, Copy)]
pub enum TouchEvent {
    Down,
    Up,
    Swipe,
    Gesture,
}

/// Holds open file descriptors for all detected touchscreen nodes.
pub struct InputReader {
    files:       Vec<File>,
    cfg:         InputModule,
    /// Screen width in ABS units (learned at runtime)
    screen_w:    i32,
    screen_h:    i32,
    // per-slot touch tracking
    tracking_id: i32,
    start_x:     i32,
    start_y:     i32,
    cur_x:       i32,
    cur_y:       i32,
    touching:    bool,
}

impl InputReader {
    pub fn new(cfg: InputModule) -> Result<Self> {
        let files = discover_touch_devices()
            .context("No touchscreen input devices found")?;
        log::info!("Input: found {} touch device(s)", files.len());
        Ok(Self {
            files,
            cfg,
            screen_w: 1080,
            screen_h: 2400,
            tracking_id: -1,
            start_x:  0,
            start_y:  0,
            cur_x:    0,
            cur_y:    0,
            touching: false,
        })
    }

    pub fn fds(&self) -> Vec<RawFd> {
        self.files.iter().map(|f| f.as_raw_fd()).collect()
    }

    /// Call when epoll signals a fd is readable.
    /// Returns zero or more events produced by the raw input.
    pub fn read_events(&mut self, fd: RawFd) -> Vec<TouchEvent> {
        // Find the matching file
        let file = match self.files.iter_mut().find(|f| f.as_raw_fd() == fd) {
            Some(f) => f,
            None    => return vec![],
        };

        let mut buf = [0u8; 24 * 32]; // up to 32 events at once
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return vec![],
        };

        let mut out = Vec::new();
        let mut i = 0;
        while i + 24 <= n {
            // Parse: 8 bytes tv_sec + 8 bytes tv_usec + 2 type + 2 code + 4 value
            let type_ = u16::from_ne_bytes([buf[i+16], buf[i+17]]);
            let code  = u16::from_ne_bytes([buf[i+18], buf[i+19]]);
            let value = i32::from_ne_bytes([buf[i+20], buf[i+21], buf[i+22], buf[i+23]]);

            if type_ == EV_ABS {
                match code {
                    ABS_MT_TRACKING_ID => {
                        if value == -1 {
                            // finger lifted
                            if self.touching {
                                self.touching = false;
                                if let Some(ev) = self.classify_touch_end() {
                                    out.push(ev);
                                }
                            }
                        } else if !self.touching {
                            self.touching    = true;
                            self.tracking_id = value;
                            self.start_x     = self.cur_x;
                            self.start_y     = self.cur_y;
                        }
                        self.tracking_id = value;
                    }
                    ABS_MT_POSITION_X | ABS_X => { self.cur_x = value; }
                    ABS_MT_POSITION_Y | ABS_Y => { self.cur_y = value; }
                    _ => {}
                }
            } else if type_ == EV_SYN && code == SYN_REPORT {
                // SYN_REPORT: snapshot is complete; if just started touching, emit Down
                if self.touching && self.start_x == self.cur_x && self.start_y == self.cur_y {
                    // first sync after finger down
                    out.push(TouchEvent::Down);
                }
            }

            i += 24;
        }
        out
    }

    fn classify_touch_end(&self) -> Option<TouchEvent> {
        let dx = (self.cur_x - self.start_x).abs() as f64 / self.screen_w as f64;
        let dy = (self.cur_y - self.start_y).abs() as f64 / self.screen_h as f64;

        let dist = (dx * dx + dy * dy).sqrt();

        // Full-screen gesture: starts near a screen edge
        let near_left   = self.start_x as f64 / self.screen_w as f64 <= self.cfg.gesture_thd_x;
        let near_right  = 1.0 - self.start_x as f64 / self.screen_w as f64 <= self.cfg.gesture_thd_x;
        let near_bottom = 1.0 - self.start_y as f64 / self.screen_h as f64 <= self.cfg.gesture_thd_y;

        if (near_left || near_right || near_bottom) && dist > self.cfg.swipe_thd {
            return Some(TouchEvent::Gesture);
        }

        if dist > self.cfg.swipe_thd {
            return Some(TouchEvent::Swipe);
        }

        Some(TouchEvent::Up)
    }
}

/// Scan /dev/input/event* and return nodes that look like touchscreens.
/// Heuristic: the device name contains "touch" or "screen" (case-insensitive),
/// or it exports ABS_MT_POSITION_X (we detect this by trying to open and
/// checking the ioctl EVIOCGBIT — but to avoid complexity we just open all
/// event nodes and let the kernel tell us via empty reads if they're not touch).
fn discover_touch_devices() -> Result<Vec<File>> {
    let mut files = Vec::new();
    let entries = fs::read_dir("/dev/input")
        .context("Cannot open /dev/input")?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if !name.starts_with("event") { continue; }

        // Try to read the device name from /sys/class/input/<name>/device/name
        let sys_name_path = format!("/sys/class/input/{}/device/name", name);
        let dev_name = fs::read_to_string(&sys_name_path)
            .unwrap_or_default()
            .to_lowercase();

        let looks_like_touch = dev_name.contains("touch")
            || dev_name.contains("screen")
            || dev_name.contains("ts_")
            || dev_name.contains("finger");

        if looks_like_touch {
            match File::open(&path) {
                Ok(f) => {
                    log::info!("Input: opened {} ({})", path.display(), dev_name.trim());
                    files.push(f);
                }
                Err(e) => log::warn!("Input: cannot open {}: {}", path.display(), e),
            }
        }
    }

    if files.is_empty() {
        // fallback: open all event nodes (will produce garbage reads for non-touch
        // devices, but the parser just ignores unrecognised events)
        log::warn!("Input: no touch device found by name, falling back to all event nodes");
        for entry in fs::read_dir("/dev/input").unwrap().flatten() {
            let path = entry.path();
            if path.file_name().unwrap_or_default()
                   .to_string_lossy().starts_with("event") {
                if let Ok(f) = File::open(&path) {
                    files.push(f);
                }
            }
        }
    }

    Ok(files)
}
