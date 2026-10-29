//! kvm-switch: one physical keyboard and mouse shared between the host (niri)
//! and a passthrough guest whose screen is a separate monitor.
//!
//! `kvm-switch-router` grabs the physical devices and replays their events on
//! one of two pairs of uinput devices: one the host compositor reads, one QEMU
//! reads (libvirt `<input type='evdev'>`, ignored by libinput). The guest thus
//! gets the input as virtual hardware, so its logon screen and UAC prompts
//! work too. Switching is requested by `kvm-switch-edge` (pushing past the
//! right edge of the host output), by the Windows guest agent (pushing past
//! the left edge of the guest desktop, over vsock), or by both Alt keys.
//!
//! This module holds the routing state machine, free of I/O, and `touchpad`
//! the touchpad's, for while the guest has the input. clipboard-bridge
//! follows the router's switches to carry the clipboard across.

use std::collections::BTreeSet;

pub mod touchpad;

/// Control socket of the router, for `kvm-switch-edge`; `KVM_SWITCH_SOCKET`
/// overrides it (for testing).
pub fn socket_path() -> String {
    std::env::var("KVM_SWITCH_SOCKET").unwrap_or_else(|_| "/run/kvm-switch/control.sock".into())
}

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;

pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Host,
    Guest,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Host => "host",
            Target::Guest => "guest",
        }
    }
}

/// Which of a target's two uinput devices an event belongs on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    Keyboard,
    Mouse,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ev {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

/// What the physical input asks the router itself to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Both Alt keys are down.
    Toggle,
    /// Both Alt keys and Backspace: give the devices back and exit.
    Quit,
}

/// Mouse buttons (BTN_MISC up to BTN_GEAR_UP) go to the mouse device, every
/// other key to the keyboard.
pub fn output_for(code: u16) -> Output {
    if (0x100..0x160).contains(&code) {
        Output::Mouse
    } else {
        Output::Keyboard
    }
}

pub struct Router {
    target: Target,
    /// Keys and buttons held down on the current target.
    down: BTreeSet<u16>,
    /// Still held physically, but already released on the target they were
    /// pressed on: their repeats and release go nowhere.
    swallowed: BTreeSet<u16>,
    left_alt: bool,
    right_alt: bool,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Router {
            target: Target::Host,
            down: BTreeSet::new(),
            swallowed: BTreeSet::new(),
            left_alt: false,
            right_alt: false,
        }
    }

    pub fn target(&self) -> Target {
        self.target
    }

    /// Routes one physical event (SYN and MSC are dropped; the caller ends
    /// each output's batch itself). Events for the current target are
    /// appended to `out`.
    pub fn input(&mut self, ev: Ev, out: &mut Vec<(Output, Ev)>) -> Option<Action> {
        match ev.kind {
            EV_REL => {
                out.push((Output::Mouse, ev));
                None
            }
            EV_KEY => {
                let action = self.hotkey(ev);
                if self.swallowed.contains(&ev.code) {
                    if ev.value == 0 {
                        self.swallowed.remove(&ev.code);
                    }
                } else {
                    match ev.value {
                        0 => {
                            self.down.remove(&ev.code);
                        }
                        1 => {
                            self.down.insert(ev.code);
                        }
                        _ => {}
                    }
                    out.push((output_for(ev.code), ev));
                }
                action
            }
            _ => None,
        }
    }

    fn hotkey(&mut self, ev: Ev) -> Option<Action> {
        let pressed = ev.value != 0;
        let both_before = self.left_alt && self.right_alt;
        match ev.code {
            KEY_LEFTALT => self.left_alt = pressed,
            KEY_RIGHTALT => self.right_alt = pressed,
            KEY_BACKSPACE if ev.value == 1 && both_before => return Some(Action::Quit),
            _ => return None,
        }
        (ev.value == 1 && !both_before && self.left_alt && self.right_alt).then_some(Action::Toggle)
    }

    /// Makes `to` the target. Returns the releases to write to the previous
    /// target for everything still held there.
    pub fn switch(&mut self, to: Target) -> Vec<(Output, Ev)> {
        if to == self.target {
            return Vec::new();
        }
        self.target = to;
        let releases = self
            .down
            .iter()
            .map(|&code| {
                (
                    output_for(code),
                    Ev {
                        kind: EV_KEY,
                        code,
                        value: 0,
                    },
                )
            })
            .collect();
        self.swallowed.append(&mut self.down);
        releases
    }

    /// Releases for everything held on the current target, before exiting.
    pub fn release_all(&mut self) -> Vec<(Output, Ev)> {
        let releases = self
            .down
            .iter()
            .map(|&code| {
                (
                    output_for(code),
                    Ev {
                        kind: EV_KEY,
                        code,
                        value: 0,
                    },
                )
            })
            .collect();
        self.down.clear();
        releases
    }
}

/// Touchpad output for the guest mouse: fractional motion and scrolling are
/// carried over to the next event, and buttons tracked so they can be
/// released when the guest loses the input.
#[derive(Default)]
pub struct GuestPointer {
    motion: (f64, f64),
    scroll: (f64, f64),
    /// Hi-res wheel units not yet worth a whole REL_WHEEL step.
    steps: (i32, i32),
    buttons: BTreeSet<u16>,
}

impl GuestPointer {
    pub fn motion(&mut self, dx: f64, dy: f64) -> Vec<Ev> {
        self.motion.0 += dx;
        self.motion.1 += dy;
        let (x, y) = (self.motion.0.trunc(), self.motion.1.trunc());
        self.motion.0 -= x;
        self.motion.1 -= y;
        [(REL_X, x as i32), (REL_Y, y as i32)]
            .into_iter()
            .filter(|&(_, v)| v != 0)
            .map(|(code, value)| Ev {
                kind: EV_REL,
                code,
                value,
            })
            .collect()
    }

    /// `v` up and `h` right, in hi-res wheel units (120 per step).
    pub fn scroll(&mut self, v: f64, h: f64) -> Vec<Ev> {
        let mut out = Vec::new();
        for (acc, steps, hi_code, code, delta) in [
            (
                &mut self.scroll.0,
                &mut self.steps.0,
                REL_WHEEL_HI_RES,
                REL_WHEEL,
                v,
            ),
            (
                &mut self.scroll.1,
                &mut self.steps.1,
                REL_HWHEEL_HI_RES,
                REL_HWHEEL,
                h,
            ),
        ] {
            *acc += delta;
            let hi = acc.trunc() as i32;
            *acc -= f64::from(hi);
            if hi == 0 {
                continue;
            }
            out.push(Ev {
                kind: EV_REL,
                code: hi_code,
                value: hi,
            });
            *steps += hi;
            let whole = *steps / 120;
            if whole != 0 {
                *steps -= whole * 120;
                out.push(Ev {
                    kind: EV_REL,
                    code,
                    value: whole,
                });
            }
        }
        out
    }

    /// Mouse buttons only (BTN_LEFT up to BTN_TASK).
    pub fn button(&mut self, code: u16, down: bool) -> Vec<Ev> {
        if !(0x110..=0x117).contains(&code) {
            return Vec::new();
        }
        let changed = if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        };
        if changed {
            vec![Ev {
                kind: EV_KEY,
                code,
                value: i32::from(down),
            }]
        } else {
            Vec::new()
        }
    }

    /// Releases for the buttons still down, when the guest loses the input.
    pub fn release_all(&mut self) -> Vec<Ev> {
        let releases = self
            .buttons
            .iter()
            .map(|&code| Ev {
                kind: EV_KEY,
                code,
                value: 0,
            })
            .collect();
        *self = GuestPointer::default();
        releases
    }
}

/// Parses a vertical position fraction sent with a switch request.
pub fn parse_fraction(s: Option<&str>) -> Option<f64> {
    s.and_then(|s| s.parse::<f64>().ok())
        .filter(|y| (0.0..=1.0).contains(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, value: i32) -> Ev {
        Ev {
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn feed(r: &mut Router, ev: Ev) -> (Vec<(Output, Ev)>, Option<Action>) {
        let mut out = Vec::new();
        let action = r.input(ev, &mut out);
        (out, action)
    }

    #[test]
    fn both_alts_toggle_and_never_reach_the_new_target() {
        let mut r = Router::new();
        assert_eq!(feed(&mut r, key(KEY_LEFTALT, 1)).1, None);
        let (out, action) = feed(&mut r, key(KEY_RIGHTALT, 1));
        assert_eq!(action, Some(Action::Toggle));
        assert_eq!(out, vec![(Output::Keyboard, key(KEY_RIGHTALT, 1))]);

        let releases = r.switch(Target::Guest);
        assert_eq!(
            releases,
            vec![
                (Output::Keyboard, key(KEY_LEFTALT, 0)),
                (Output::Keyboard, key(KEY_RIGHTALT, 0)),
            ]
        );
        // Repeats and releases of the swallowed Alts go nowhere.
        assert!(feed(&mut r, key(KEY_LEFTALT, 2)).0.is_empty());
        assert!(feed(&mut r, key(KEY_LEFTALT, 0)).0.is_empty());
        assert!(feed(&mut r, key(KEY_RIGHTALT, 0)).0.is_empty());
        // A fresh press does.
        assert_eq!(
            feed(&mut r, key(30, 1)).0,
            vec![(Output::Keyboard, key(30, 1))]
        );
    }

    #[test]
    fn toggle_fires_once_per_chord() {
        let mut r = Router::new();
        feed(&mut r, key(KEY_LEFTALT, 1));
        assert_eq!(feed(&mut r, key(KEY_RIGHTALT, 1)).1, Some(Action::Toggle));
        assert_eq!(feed(&mut r, key(KEY_RIGHTALT, 2)).1, None);
        assert_eq!(feed(&mut r, key(KEY_LEFTALT, 2)).1, None);
    }

    #[test]
    fn quit_needs_both_alts() {
        let mut r = Router::new();
        assert_eq!(feed(&mut r, key(KEY_BACKSPACE, 1)).1, None);
        feed(&mut r, key(KEY_BACKSPACE, 0));
        feed(&mut r, key(KEY_LEFTALT, 1));
        feed(&mut r, key(KEY_RIGHTALT, 1));
        assert_eq!(feed(&mut r, key(KEY_BACKSPACE, 1)).1, Some(Action::Quit));
    }

    #[test]
    fn buttons_and_motion_go_to_the_mouse() {
        let mut r = Router::new();
        let motion = Ev {
            kind: EV_REL,
            code: 0,
            value: 5,
        };
        assert_eq!(feed(&mut r, motion).0, vec![(Output::Mouse, motion)]);
        assert_eq!(
            feed(&mut r, key(0x110, 1)).0,
            vec![(Output::Mouse, key(0x110, 1))]
        );
        assert_eq!(
            r.switch(Target::Guest),
            vec![(Output::Mouse, key(0x110, 0))]
        );
        assert!(feed(&mut r, key(0x110, 0)).0.is_empty());
    }

    #[test]
    fn switching_to_the_current_target_changes_nothing() {
        let mut r = Router::new();
        feed(&mut r, key(30, 1));
        assert!(r.switch(Target::Host).is_empty());
        assert_eq!(
            feed(&mut r, key(30, 0)).0,
            vec![(Output::Keyboard, key(30, 0))]
        );
    }

    #[test]
    fn guest_pointer_motion_carries_fractions() {
        let mut f = GuestPointer::default();
        assert!(f.motion(0.6, -0.4).is_empty());
        assert_eq!(
            f.motion(0.6, -0.7),
            vec![
                Ev {
                    kind: EV_REL,
                    code: REL_X,
                    value: 1
                },
                Ev {
                    kind: EV_REL,
                    code: REL_Y,
                    value: -1
                },
            ]
        );
    }

    #[test]
    fn guest_pointer_scroll_steps_every_120_units() {
        let mut f = GuestPointer::default();
        assert_eq!(
            f.scroll(100.0, 0.0),
            vec![Ev {
                kind: EV_REL,
                code: REL_WHEEL_HI_RES,
                value: 100
            }]
        );
        assert_eq!(
            f.scroll(40.0, 0.0),
            vec![
                Ev {
                    kind: EV_REL,
                    code: REL_WHEEL_HI_RES,
                    value: 40
                },
                Ev {
                    kind: EV_REL,
                    code: REL_WHEEL,
                    value: 1
                },
            ]
        );
        assert_eq!(
            f.scroll(0.0, -120.0),
            vec![
                Ev {
                    kind: EV_REL,
                    code: REL_HWHEEL_HI_RES,
                    value: -120
                },
                Ev {
                    kind: EV_REL,
                    code: REL_HWHEEL,
                    value: -1
                },
            ]
        );
    }

    #[test]
    fn guest_pointer_buttons_are_released_on_leave() {
        let mut f = GuestPointer::default();
        assert_eq!(f.button(0x110, true), vec![key(0x110, 1)]);
        assert!(f.button(0x110, true).is_empty());
        assert!(f.button(30, true).is_empty());
        assert_eq!(f.release_all(), vec![key(0x110, 0)]);
        assert!(f.button(0x110, false).is_empty());
    }

    #[test]
    fn fractions_are_clamped_to_the_unit_range() {
        assert_eq!(parse_fraction(Some("0.25")), Some(0.25));
        assert_eq!(parse_fraction(Some("1.5")), None);
        assert_eq!(parse_fraction(Some("x")), None);
        assert_eq!(parse_fraction(None), None);
    }
}
