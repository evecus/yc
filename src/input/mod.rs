// input/mod.rs
// Linux evdev touch reader.
//
// 支持两种 MT 协议：
//   Type A — 无 slot，靠 ABS_MT_TRACKING_ID=-1 判断 lift
//   Type B — 有 ABS_MT_SLOT，每个 slot 独立跟踪（现代触摸屏主流）
//
// 本机 nvtcapacitivetouchscreen 使用 Type B（有 ABS_MT_SLOT bit 47）

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use anyhow::{Result, Context};
use crate::config::InputModule;

// linux/input-event-codes.h
const EV_ABS: u16 = 3;
const EV_SYN: u16 = 0;
const SYN_REPORT: u16 = 0;

const ABS_MT_SLOT:         u16 = 47;
const ABS_MT_POSITION_X:   u16 = 53;
const ABS_MT_POSITION_Y:   u16 = 54;
const ABS_MT_TRACKING_ID:  u16 = 57;
// Type A fallback
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;

const MAX_SLOTS: usize = 10;

#[derive(Debug, Clone, Copy)]
pub enum TouchEvent {
    Down,
    Up,
    Swipe,
    Gesture,
}

// 单个 MT slot 状态
#[derive(Clone, Copy, Default)]
struct Slot {
    tracking_id: i32,   // -1 = 空闲
    x: i32,
    y: i32,
    start_x: i32,
    start_y: i32,
    active: bool,       // 本次 SYN_REPORT 前是否有手指
}

pub struct InputReader {
    files:     Vec<File>,
    cfg:       InputModule,
    screen_w:  i32,
    screen_h:  i32,
    // Type B state
    slots:        [Slot; MAX_SLOTS],
    cur_slot:     usize,
    // pending changes in this frame
    slot_changed: [bool; MAX_SLOTS],
}

impl InputReader {
    pub fn new(cfg: InputModule) -> Result<Self> {
        let files = discover_touch_devices()
            .context("No touchscreen input devices found")?;
        log::info!("Input: found {} touch device(s)", files.len());

        let mut slots = [Slot::default(); MAX_SLOTS];
        for s in slots.iter_mut() { s.tracking_id = -1; }

        Ok(Self {
            files,
            cfg,
            screen_w: 1080,
            screen_h: 2400,
            slots,
            cur_slot: 0,
            slot_changed: [false; MAX_SLOTS],
        })
    }

    pub fn fds(&self) -> Vec<RawFd> {
        self.files.iter().map(|f| f.as_raw_fd()).collect()
    }

    pub fn read_events(&mut self, fd: RawFd) -> Vec<TouchEvent> {
        let file = match self.files.iter_mut().find(|f| f.as_raw_fd() == fd) {
            Some(f) => f,
            None    => return vec![],
        };

        // aarch64: input_event = 8+8+2+2+4 = 24 bytes
        let mut buf = [0u8; 24 * 64];
        let n = match file.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => return vec![],
        };

        let mut out = Vec::new();
        let mut i = 0;

        while i + 24 <= n {
            let ev_type  = u16::from_ne_bytes([buf[i+16], buf[i+17]]);
            let ev_code  = u16::from_ne_bytes([buf[i+18], buf[i+19]]);
            let ev_value = i32::from_ne_bytes([buf[i+20], buf[i+21], buf[i+22], buf[i+23]]);
            i += 24;

            match ev_type {
                EV_ABS => self.handle_abs(ev_code, ev_value),
                EV_SYN if ev_code == SYN_REPORT => {
                    self.handle_syn(&mut out);
                }
                _ => {}
            }
        }
        out
    }

    // ── ABS event 积累 ───────────────────────────────────────────────────────

    fn handle_abs(&mut self, code: u16, value: i32) {
        match code {
            ABS_MT_SLOT => {
                if (value as usize) < MAX_SLOTS {
                    self.cur_slot = value as usize;
                }
            }
            ABS_MT_TRACKING_ID => {
                let s = &mut self.slots[self.cur_slot];
                s.tracking_id = value;
                self.slot_changed[self.cur_slot] = true;
                if value != -1 && !s.active {
                    // finger just placed — record start position
                    s.start_x = s.x;
                    s.start_y = s.y;
                }
            }
            ABS_MT_POSITION_X | ABS_X => {
                self.slots[self.cur_slot].x = value;
                // Update screen width estimate
                if value > self.screen_w { self.screen_w = value + 1; }
            }
            ABS_MT_POSITION_Y | ABS_Y => {
                self.slots[self.cur_slot].y = value;
                if value > self.screen_h { self.screen_h = value + 1; }
            }
            _ => {}
        }
    }

    // ── SYN_REPORT: commit frame ─────────────────────────────────────────────

    fn handle_syn(&mut self, out: &mut Vec<TouchEvent>) {
        for idx in 0..MAX_SLOTS {
            if !self.slot_changed[idx] { continue; }
            self.slot_changed[idx] = false;

            let s = &mut self.slots[idx];
            let was_active  = s.active;
            let now_active  = s.tracking_id != -1;

            if !was_active && now_active {
                // finger down
                s.active  = true;
                s.start_x = s.x;
                s.start_y = s.y;
                out.push(TouchEvent::Down);
                log::trace!("touch down slot={idx} ({},{})", s.x, s.y);

            } else if was_active && !now_active {
                // finger up — classify gesture
                s.active = false;
                if let Some(ev) = classify(s, self.screen_w, self.screen_h, &self.cfg) {
                    log::trace!("touch up slot={idx} → {:?}", ev);
                    out.push(ev);
                }
            }
        }
    }
}

// ── Gesture classification ───────────────────────────────────────────────────

fn classify(s: &Slot, sw: i32, sh: i32, cfg: &InputModule) -> Option<TouchEvent> {
    let dx = (s.x - s.start_x).abs() as f64 / sw as f64;
    let dy = (s.y - s.start_y).abs() as f64 / sh as f64;
    let dist = (dx * dx + dy * dy).sqrt();

    let near_left   = s.start_x as f64 / sw as f64 <= cfg.gesture_thd_x;
    let near_right  = 1.0 - s.start_x as f64 / sw as f64 <= cfg.gesture_thd_x;
    let near_bottom = 1.0 - s.start_y as f64 / sh as f64 <= cfg.gesture_thd_y;

    if (near_left || near_right || near_bottom) && dist > cfg.swipe_thd {
        return Some(TouchEvent::Gesture);
    }
    if dist > cfg.swipe_thd {
        return Some(TouchEvent::Swipe);
    }
    Some(TouchEvent::Up)
}

// ── Device discovery ─────────────────────────────────────────────────────────

fn discover_touch_devices() -> Result<Vec<File>> {
    let mut files = Vec::new();
    let entries = fs::read_dir("/dev/input")
        .context("Cannot open /dev/input")?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default()
                       .to_string_lossy().into_owned();
        if !name.starts_with("event") { continue; }

        let sys_name = format!("/sys/class/input/{}/device/name", name);
        let dev_name = fs::read_to_string(&sys_name)
            .unwrap_or_default().to_lowercase();

        // 只要触摸屏，排除手写笔（pen only 没有 MT 事件）
        let is_touch = dev_name.contains("touch") || dev_name.contains("ts_")
                    || dev_name.contains("finger");
        let is_pen_only = dev_name.contains("pen") && !dev_name.contains("touch");

        if is_touch && !is_pen_only {
            // 验证有 ABS_MT_POSITION_X (bit 53) — 读 capabilities/abs
            let abs_cap_path = format!("/sys/class/input/{}/device/capabilities/abs", name);
            let has_mt = fs::read_to_string(&abs_cap_path)
                .map(|s| {
                    // hex bitmap，bit 53 set 表示有 ABS_MT_POSITION_X
                    u128::from_str_radix(s.trim(), 16)
                        .map(|v| v & (1u128 << 53) != 0)
                        .unwrap_or(false)
                })
                .unwrap_or(true); // 读不到就假设有

            if has_mt {
                match File::open(&path) {
                    Ok(f) => {
                        log::info!("Input: opened {} ({})", path.display(), dev_name.trim());
                        files.push(f);
                    }
                    Err(e) => log::warn!("Input: cannot open {}: {}", path.display(), e),
                }
            } else {
                log::debug!("Input: skipping {} (no ABS_MT_POSITION_X)", name);
            }
        }
    }

    if files.is_empty() {
        anyhow::bail!("no multitouch devices found");
    }
    Ok(files)
}
