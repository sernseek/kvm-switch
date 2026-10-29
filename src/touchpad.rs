//! The touchpad while the guest has the input. The guest gets a plain
//! relative mouse (QEMU cannot present a Windows precision touchpad), so the
//! router grabs the touchpad and does what libinput would: one finger moves,
//! two scroll (naturally, like niri's default), taps click (one finger left,
//! two right, three middle), pressing the pad clicks by finger count, and
//! three- and four-finger swipes become the Windows shortcuts for the same
//! gestures. niri sees none of it, so its own swipe gestures stay quiet.

use crate::{EV_KEY, Ev};

pub const EV_ABS: u16 = 0x03;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

const KEY_TAB: u16 = 15;
const KEY_LEFTCTRL: u16 = 29;
const KEY_D: u16 = 32;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_LEFTMETA: u16 = 125;

/// Mouse counts per mm of finger travel: what libinput normalizes touchpads
/// to (a 1000 dpi mouse). The guest applies its own pointer acceleration.
const COUNTS_PER_MM: f64 = 1000.0 / 25.4;
/// Hi-res wheel units (120 per step) per mm of two-finger travel.
const WHEEL_PER_MM: f64 = 24.0;
/// A touch shorter than this, that moved less than `TAP_MM`, is a tap.
const TAP_MS: u64 = 200;
const TAP_MM: f64 = 3.0;
/// Travel that makes three or four fingers a swipe.
const SWIPE_MM: f64 = 15.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TouchOut {
    /// Relative mouse counts.
    Motion(f64, f64),
    /// Hi-res wheel units, `v` up and `h` right.
    Scroll(f64, f64),
    Button(u16, bool),
    /// A shortcut: pressed in order, released in reverse.
    Keys(&'static [u16]),
}

#[derive(Default)]
struct Session {
    start_ms: u64,
    max_fingers: usize,
    moved_mm: f64,
    /// No tap at the end: the pad was pressed, a swipe fired, or the touch
    /// began before the guest got the input.
    no_tap: bool,
    /// A swipe fired: ignore the rest of the touch.
    swiped: bool,
    swipe: (f64, f64),
}

pub struct Touchpad {
    /// Device units per mm.
    res: (f64, f64),
    slot: usize,
    slots: Vec<Option<(i32, i32)>>,
    fingers: usize,
    /// Centroid in mm at the previous frame, for the current finger count.
    last: Option<(f64, f64)>,
    session: Option<Session>,
    /// The button the pad press was mapped to, while it is down.
    pressed: Option<u16>,
}

impl Touchpad {
    pub fn new(res_x: f64, res_y: f64, slots: usize) -> Touchpad {
        Touchpad {
            res: (res_x.max(1.0), res_y.max(1.0)),
            slot: 0,
            slots: vec![None; slots.max(1)],
            fingers: 0,
            last: None,
            session: None,
            pressed: None,
        }
    }

    /// The guest just got the input: a touch already in progress may move
    /// the pointer but not end in a tap.
    pub fn begin(&mut self) {
        self.last = None;
        if let Some(s) = &mut self.session {
            s.no_tap = true;
        }
    }

    /// The guest is losing the input: what must be released there.
    pub fn end(&mut self) -> Vec<TouchOut> {
        self.last = None;
        if let Some(s) = &mut self.session {
            s.no_tap = true;
        }
        self.pressed
            .take()
            .map(|b| TouchOut::Button(b, false))
            .into_iter()
            .collect()
    }

    fn centroid(&self) -> Option<(f64, f64)> {
        let active: Vec<_> = self.slots.iter().flatten().collect();
        if active.is_empty() {
            return None;
        }
        let n = active.len() as f64;
        let (sx, sy) = active.iter().fold((0.0, 0.0), |(sx, sy), (x, y)| {
            (sx + f64::from(*x), sy + f64::from(*y))
        });
        Some((sx / n / self.res.0, sy / n / self.res.1))
    }

    /// Processes one SYN frame. Always called, also while the host has the
    /// input, so the touch state is right when the guest gets it.
    pub fn frame(&mut self, events: &[Ev], time_ms: u64) -> Vec<TouchOut> {
        let mut out = Vec::new();
        let mut pad = None;
        for e in events {
            match (e.kind, e.code) {
                (EV_ABS, ABS_MT_SLOT) => self.slot = e.value.max(0) as usize,
                (EV_ABS, ABS_MT_TRACKING_ID) => {
                    if let Some(s) = self.slots.get_mut(self.slot) {
                        *s = (e.value >= 0).then(|| s.unwrap_or((0, 0)));
                    }
                }
                (EV_ABS, ABS_MT_POSITION_X) => {
                    if let Some(Some(p)) = self.slots.get_mut(self.slot) {
                        p.0 = e.value;
                    }
                }
                (EV_ABS, ABS_MT_POSITION_Y) => {
                    if let Some(Some(p)) = self.slots.get_mut(self.slot) {
                        p.1 = e.value;
                    }
                }
                (EV_KEY, BTN_LEFT) => pad = Some(e.value != 0),
                _ => {}
            }
        }

        let fingers = self.slots.iter().flatten().count();
        let centroid = self.centroid();
        if fingers > 0 && self.session.is_none() {
            self.session = Some(Session {
                start_ms: time_ms,
                ..Session::default()
            });
        }

        match pad {
            Some(true) if self.pressed.is_none() => {
                let button = match fingers {
                    0 | 1 => BTN_LEFT,
                    2 => BTN_RIGHT,
                    _ => BTN_MIDDLE,
                };
                self.pressed = Some(button);
                out.push(TouchOut::Button(button, true));
                if let Some(s) = &mut self.session {
                    s.no_tap = true;
                }
            }
            Some(false) => {
                if let Some(button) = self.pressed.take() {
                    out.push(TouchOut::Button(button, false));
                }
            }
            _ => {}
        }

        if fingers != self.fingers {
            // A finger landed or lifted: restart motion from here, so the
            // centroid jump is not motion, and restart any swipe.
            self.fingers = fingers;
            self.last = centroid;
            if let Some(s) = &mut self.session {
                s.max_fingers = s.max_fingers.max(fingers);
                s.swipe = (0.0, 0.0);
            }
        } else if let (Some(c), Some(l), Some(s)) = (centroid, self.last, &mut self.session) {
            let d = (c.0 - l.0, c.1 - l.1);
            self.last = Some(c);
            s.moved_mm += d.0.hypot(d.1);
            if !s.swiped && d != (0.0, 0.0) {
                match fingers {
                    1 => out.push(TouchOut::Motion(d.0 * COUNTS_PER_MM, d.1 * COUNTS_PER_MM)),
                    2 => out.push(TouchOut::Scroll(d.1 * WHEEL_PER_MM, -d.0 * WHEEL_PER_MM)),
                    _ => {
                        s.swipe.0 += d.0;
                        s.swipe.1 += d.1;
                        if let Some(keys) = swipe_keys(fingers, s.swipe) {
                            s.swiped = true;
                            s.no_tap = true;
                            out.push(TouchOut::Keys(keys));
                        }
                    }
                }
            }
        }

        if fingers == 0 {
            self.last = None;
            if let Some(s) = self.session.take() {
                let quick = time_ms.saturating_sub(s.start_ms) <= TAP_MS;
                if quick && !s.no_tap && s.moved_mm < TAP_MM {
                    let button = match s.max_fingers {
                        1 => Some(BTN_LEFT),
                        2 => Some(BTN_RIGHT),
                        3 => Some(BTN_MIDDLE),
                        _ => None,
                    };
                    if let Some(b) = button {
                        out.push(TouchOut::Button(b, true));
                        out.push(TouchOut::Button(b, false));
                    }
                }
            }
        }
        out
    }
}

/// Windows' own bindings for precision touchpad swipes. Content follows the
/// fingers: swiping four fingers left shows the desktop to the right.
fn swipe_keys(fingers: usize, (dx, dy): (f64, f64)) -> Option<&'static [u16]> {
    if dx.abs().max(dy.abs()) < SWIPE_MM {
        return None;
    }
    let horizontal = dx.abs() > dy.abs();
    Some(match (fingers >= 4, horizontal, dx > 0.0, dy > 0.0) {
        (_, false, _, false) => &[KEY_LEFTMETA, KEY_TAB],
        (_, false, _, true) => &[KEY_LEFTMETA, KEY_D],
        (false, true, true, _) => &[KEY_LEFTALT, KEY_TAB],
        (false, true, false, _) => &[KEY_LEFTALT, KEY_LEFTSHIFT, KEY_TAB],
        (true, true, true, _) => &[KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFT],
        (true, true, false, _) => &[KEY_LEFTCTRL, KEY_LEFTMETA, KEY_RIGHT],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RES: f64 = 10.0; // units per mm

    fn abs(code: u16, value: i32) -> Ev {
        Ev {
            kind: EV_ABS,
            code,
            value,
        }
    }

    /// One frame moving the given slots to (x, y) mm; None lifts the slot.
    fn touch(tp: &mut Touchpad, fingers: &[(usize, Option<(f64, f64)>)], t: u64) -> Vec<TouchOut> {
        let mut evs = Vec::new();
        for &(slot, pos) in fingers {
            evs.push(abs(ABS_MT_SLOT, slot as i32));
            match pos {
                Some((x, y)) => {
                    if tp.slots[slot].is_none() {
                        evs.push(abs(ABS_MT_TRACKING_ID, slot as i32 + 1));
                    }
                    evs.push(abs(ABS_MT_POSITION_X, (x * RES) as i32));
                    evs.push(abs(ABS_MT_POSITION_Y, (y * RES) as i32));
                }
                None => evs.push(abs(ABS_MT_TRACKING_ID, -1)),
            }
        }
        tp.frame(&evs, t)
    }

    fn pad() -> Touchpad {
        Touchpad::new(RES, RES, 5)
    }

    #[test]
    fn one_finger_moves_without_a_jump_on_landing() {
        let mut tp = pad();
        assert!(touch(&mut tp, &[(0, Some((50.0, 50.0)))], 0).is_empty());
        let out = touch(&mut tp, &[(0, Some((51.0, 50.0)))], 10);
        let [TouchOut::Motion(dx, dy)] = out[..] else {
            panic!("{out:?}")
        };
        assert!((dx - COUNTS_PER_MM).abs() < 1e-9 && dy == 0.0);
    }

    #[test]
    fn a_quick_touch_taps_by_finger_count() {
        let mut tp = pad();
        touch(&mut tp, &[(0, Some((50.0, 50.0)))], 0);
        assert_eq!(
            touch(&mut tp, &[(0, None)], 80),
            vec![
                TouchOut::Button(BTN_LEFT, true),
                TouchOut::Button(BTN_LEFT, false)
            ]
        );
        touch(
            &mut tp,
            &[(0, Some((50.0, 50.0))), (1, Some((60.0, 50.0)))],
            200,
        );
        assert_eq!(
            touch(&mut tp, &[(0, None), (1, None)], 260),
            vec![
                TouchOut::Button(BTN_RIGHT, true),
                TouchOut::Button(BTN_RIGHT, false)
            ]
        );
        // Too long is no tap.
        touch(&mut tp, &[(0, Some((50.0, 50.0)))], 1000);
        assert!(touch(&mut tp, &[(0, None)], 1500).is_empty());
    }

    #[test]
    fn two_fingers_scroll_naturally() {
        let mut tp = pad();
        touch(
            &mut tp,
            &[(0, Some((50.0, 50.0))), (1, Some((60.0, 50.0)))],
            0,
        );
        // Fingers up: content moves up, i.e. the wheel scrolls down.
        let out = touch(
            &mut tp,
            &[(0, Some((50.0, 48.0))), (1, Some((60.0, 48.0)))],
            10,
        );
        assert_eq!(out, vec![TouchOut::Scroll(-2.0 * WHEEL_PER_MM, 0.0)]);
    }

    #[test]
    fn swipes_fire_once_and_map_to_shortcuts() {
        let mut tp = pad();
        let three = |y: f64| {
            [
                (0, Some((40.0, y))),
                (1, Some((50.0, y))),
                (2, Some((60.0, y))),
            ]
        };
        touch(&mut tp, &three(50.0), 0);
        assert!(touch(&mut tp, &three(40.0), 10).is_empty());
        assert_eq!(
            touch(&mut tp, &three(30.0), 20),
            vec![TouchOut::Keys(&[KEY_LEFTMETA, KEY_TAB])]
        );
        assert!(touch(&mut tp, &three(10.0), 30).is_empty());
        assert!(touch(&mut tp, &[(0, None), (1, None), (2, None)], 40).is_empty());

        assert_eq!(
            swipe_keys(4, (-20.0, 1.0)),
            Some(&[KEY_LEFTCTRL, KEY_LEFTMETA, KEY_RIGHT][..])
        );
        assert_eq!(swipe_keys(3, (0.0, 20.0)), Some(&[KEY_LEFTMETA, KEY_D][..]));
    }

    #[test]
    fn pressing_the_pad_clicks_by_finger_count_and_is_no_tap() {
        let mut tp = pad();
        touch(
            &mut tp,
            &[(0, Some((50.0, 50.0))), (1, Some((60.0, 50.0)))],
            0,
        );
        let press = Ev {
            kind: EV_KEY,
            code: BTN_LEFT,
            value: 1,
        };
        assert_eq!(
            tp.frame(&[press], 10),
            vec![TouchOut::Button(BTN_RIGHT, true)]
        );
        let release = Ev {
            kind: EV_KEY,
            code: BTN_LEFT,
            value: 0,
        };
        assert_eq!(
            tp.frame(&[release], 20),
            vec![TouchOut::Button(BTN_RIGHT, false)]
        );
        assert!(touch(&mut tp, &[(0, None), (1, None)], 30).is_empty());
    }

    #[test]
    fn end_releases_a_held_press() {
        let mut tp = pad();
        touch(&mut tp, &[(0, Some((50.0, 50.0)))], 0);
        tp.frame(
            &[Ev {
                kind: EV_KEY,
                code: BTN_LEFT,
                value: 1,
            }],
            10,
        );
        assert_eq!(tp.end(), vec![TouchOut::Button(BTN_LEFT, false)]);
        assert!(tp.end().is_empty());
    }
}
