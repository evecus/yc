// fsm/mod.rs
// Scene / hint state machine.
//
// States:  idle → touch → trigger / gesture / switch / junk → touch → idle
//
// Each state has a maximum duration after which it falls back to the
// predecessor.  On entry to a new state the sysfs writer and scheduler
// are notified to apply the matching preset parameters.

use std::time::{Duration, Instant};
use crate::config::HintDuration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum Hint {
    Idle,
    Touch,
    Trigger,
    Gesture,
    Switch,
    Junk,
}

impl Hint {
    pub fn as_str(self) -> &'static str {
        match self {
            Hint::Idle    => "idle",
            Hint::Touch   => "touch",
            Hint::Trigger => "trigger",
            Hint::Gesture => "gesture",
            Hint::Switch  => "switch",
            Hint::Junk    => "junk",
        }
    }

    /// The scheduler scene that each hint maps to (matches original behaviour)
    pub fn sched_scene(self) -> &'static str {
        match self {
            Hint::Idle               => "idle",
            Hint::Touch | Hint::Trigger | Hint::Gesture | Hint::Junk => "touch",
            Hint::Switch             => "boost",
        }
    }
}

pub struct StateMachine {
    state:      Hint,
    entered_at: Instant,
    durations:  HintDuration,
}

impl StateMachine {
    pub fn new(durations: HintDuration) -> Self {
        Self {
            state:      Hint::Idle,
            entered_at: Instant::now(),
            durations,
        }
    }

    pub fn current(&self) -> Hint { self.state }

    /// Called by the event loop to expire timed states.
    /// Returns Some(new_hint) when a transition occurs.
    pub fn tick(&mut self) -> Option<Hint> {
        let elapsed = self.entered_at.elapsed().as_secs_f64();
        let timeout = self.timeout_secs(self.state);
        if timeout > 0.0 && elapsed >= timeout {
            let next = self.fallback(self.state);
            self.enter(next);
            Some(next)
        } else {
            None
        }
    }

    /// External events drive transitions.
    pub fn on_touch_down(&mut self) -> Option<Hint> {
        match self.state {
            Hint::Idle => Some(self.enter(Hint::Touch)),
            _          => None,
        }
    }

    pub fn on_touch_up(&mut self) -> Option<Hint> {
        // touch-up from Touch → Trigger
        match self.state {
            Hint::Touch => Some(self.enter(Hint::Trigger)),
            _           => None,
        }
    }

    pub fn on_swipe(&mut self) -> Option<Hint> {
        match self.state {
            Hint::Touch => Some(self.enter(Hint::Trigger)),
            _           => None,
        }
    }

    pub fn on_gesture(&mut self) -> Option<Hint> {
        match self.state {
            Hint::Touch => Some(self.enter(Hint::Gesture)),
            _           => None,
        }
    }

    pub fn on_window_switch(&mut self) -> Option<Hint> {
        // App switch always overrides current state
        Some(self.enter(Hint::Switch))
    }

    pub fn on_screen_on(&mut self) -> Option<Hint> {
        Some(self.enter(Hint::Switch))
    }

    pub fn on_screen_off(&mut self) -> Option<Hint> {
        Some(self.enter(Hint::Idle))
    }

    #[allow(dead_code)]
    pub fn on_junk(&mut self) -> Option<Hint> {
        match self.state {
            Hint::Touch | Hint::Gesture => Some(self.enter(Hint::Junk)),
            _                           => None,
        }
    }

    // ── internal ─────────────────────────────────────────────────────────────

    fn enter(&mut self, next: Hint) -> Hint {
        log::debug!("FSM: {:?} → {:?}", self.state, next);
        self.state      = next;
        self.entered_at = Instant::now();
        next
    }

    fn timeout_secs(&self, h: Hint) -> f64 {
        match h {
            Hint::Idle    => self.durations.idle,
            Hint::Touch   => self.durations.touch,
            Hint::Trigger => self.durations.trigger,
            Hint::Gesture => self.durations.gesture,
            Hint::Switch  => self.durations.switch,
            Hint::Junk    => self.durations.junk,
        }
    }

    fn fallback(&self, h: Hint) -> Hint {
        match h {
            Hint::Idle               => Hint::Idle,
            Hint::Trigger | Hint::Gesture | Hint::Switch | Hint::Junk => Hint::Touch,
            Hint::Touch              => Hint::Idle,
        }
    }

    /// Minimum sleep until the next possible timeout (for epoll timeout).
    pub fn next_timeout(&self) -> Duration {
        let timeout = self.timeout_secs(self.state);
        if timeout <= 0.0 {
            return Duration::from_secs(10);
        }
        let elapsed = self.entered_at.elapsed().as_secs_f64();
        let remaining = (timeout - elapsed).max(0.001);
        Duration::from_secs_f64(remaining)
    }
}
