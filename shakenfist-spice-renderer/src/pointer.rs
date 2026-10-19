//! Pointer translator: turns frontend pointer positions into the
//! [`InputEvent`]s the inputs channel sends.
//!
//! Which pointer message a SPICE server acts on depends on the
//! negotiated mouse mode, and it discards the wrong one without
//! saying anything. Client mode means the guest has a vdagent and
//! therefore an absolute pointing device, so `MOUSE_POSITION` lands.
//! Server mode means it does not, and only relative `MOUSE_MOTION` is
//! consumed: absolute positions sent to a server-mode session move
//! nothing at all, which presents as a dead pointer rather than as an
//! error.
//!
//! [`PointerTranslator`] holds only the last position sent and takes
//! the mouse mode as an argument on every call, so a frontend that
//! reads the mode from a field and one that reads it from a `watch`
//! can both use it, and a mode change part-way through a session
//! takes effect on the next call. Coordinates are surface pixels.

use shakenfist_spice_protocol::{mouse_buttons, MOUSE_MODE_SERVER};

use crate::channels::InputEvent;

/// Derives absolute positions or relative deltas from successive
/// pointer positions, and places button and wheel events.
#[derive(Debug, Default)]
pub struct PointerTranslator {
    last_pos: Option<(u32, u32)>,
}

impl PointerTranslator {
    /// A translator with no position yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the last position, as on a reconnect: the next server
    /// mode move is then a fresh reference point rather than a jump
    /// from wherever the pointer was in the previous session.
    pub fn reset(&mut self) {
        self.last_pos = None;
    }

    /// The last position sent or recorded, if any.
    pub fn last_pos(&self) -> Option<(u32, u32)> {
        self.last_pos
    }

    /// The event for a pointer move to `(x, y)`, or `None` when there
    /// is nothing to tell the guest.
    ///
    /// A move to the last position sends nothing in either mode. In
    /// server mode that matters: every `MOUSE_MOTION` takes a slot in
    /// the ack window, which only drains on `MOUSE_MOTION_ACK`, and
    /// frontends report sub-pixel movement that lands on the same
    /// pixel, so zero deltas would be a steady trickle rather than a
    /// rarity. In client mode the guest ignores a repeated position
    /// anyway.
    pub fn motion(&mut self, mouse_mode: u32, x: u32, y: u32) -> Option<InputEvent> {
        let prev = self.last_pos.replace((x, y));
        if prev == Some((x, y)) {
            return None;
        }
        if mouse_mode == MOUSE_MODE_SERVER {
            // The first move after a reset has no reference point.
            // Treat it as a zero delta, and so send nothing, rather
            // than as a jump from the origin.
            let (prev_x, prev_y) = prev?;
            Some(InputEvent::MouseMotion {
                dx: x as i32 - prev_x as i32,
                dy: y as i32 - prev_y as i32,
            })
        } else {
            Some(InputEvent::MouseMove { x, y })
        }
    }

    /// The events for a press (`down`) or release of the SPICE button
    /// mask `button`: at most a catch-up motion, then the button
    /// event.
    ///
    /// `pos` is the button's own position for a frontend whose button
    /// events carry one, and `None` to place it at the last position.
    /// A position is recorded as the new reference point, so a move
    /// after a click-without-move is not measured from a stale one.
    ///
    /// In server mode, a position that differs from the last one is
    /// first sent as the `MouseMotion` delta to it, because
    /// `MOUSE_PRESS` and `MOUSE_RELEASE` carry no position and the
    /// movement would otherwise be lost. Client mode needs no extra
    /// event: the inputs channel sends `MOUSE_POSITION` before every
    /// press and release, as spice-gtk does.
    pub fn button(
        &mut self,
        mouse_mode: u32,
        button: u32,
        down: bool,
        pos: Option<(u32, u32)>,
    ) -> Vec<InputEvent> {
        let mut events = Vec::with_capacity(2);
        let (x, y) = match pos {
            Some((x, y)) => {
                if mouse_mode == MOUSE_MODE_SERVER {
                    if let Some(motion) = self.motion(mouse_mode, x, y) {
                        events.push(motion);
                    }
                } else {
                    self.last_pos = Some((x, y));
                }
                (x, y)
            }
            None => self.last_pos.unwrap_or((0, 0)),
        };
        events.push(if down {
            InputEvent::MouseDown { button, x, y }
        } else {
            InputEvent::MouseUp { button, x, y }
        });
        events
    }

    /// Turn signed wheel `clicks` (positive is up) into button 4 or 5
    /// press-release pairs at the last position, one pair per click.
    pub fn wheel(&self, clicks: i32) -> Vec<InputEvent> {
        let button = if clicks > 0 {
            mouse_buttons::UP
        } else {
            mouse_buttons::DOWN
        };
        let (x, y) = self.last_pos.unwrap_or((0, 0));
        (0..clicks.unsigned_abs())
            .flat_map(|_| {
                [
                    InputEvent::MouseDown { button, x, y },
                    InputEvent::MouseUp { button, x, y },
                ]
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_protocol::MOUSE_MODE_CLIENT;

    const SERVER: u32 = MOUSE_MODE_SERVER;
    const CLIENT: u32 = MOUSE_MODE_CLIENT;

    #[test]
    fn client_mode_sends_absolute_positions() {
        let mut p = PointerTranslator::new();
        assert!(matches!(
            p.motion(CLIENT, 10, 20),
            Some(InputEvent::MouseMove { x: 10, y: 20 })
        ));
        assert!(matches!(
            p.motion(CLIENT, 15, 5),
            Some(InputEvent::MouseMove { x: 15, y: 5 })
        ));
        assert_eq!(p.last_pos(), Some((15, 5)));
    }

    #[test]
    fn a_repeated_position_sends_nothing_in_client_mode() {
        let mut p = PointerTranslator::new();
        assert!(p.motion(CLIENT, 10, 20).is_some());
        assert!(p.motion(CLIENT, 10, 20).is_none());
    }

    #[test]
    fn a_repeated_position_sends_nothing_in_server_mode() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 10, 20);
        assert!(p.motion(SERVER, 12, 20).is_some());
        assert!(p.motion(SERVER, 12, 20).is_none());
    }

    #[test]
    fn the_first_server_mode_move_sends_nothing() {
        let mut p = PointerTranslator::new();
        assert!(p.motion(SERVER, 100, 200).is_none());
        assert_eq!(p.last_pos(), Some((100, 200)));
    }

    #[test]
    fn server_mode_sends_deltas_from_the_last_position() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 100, 200);
        assert!(matches!(
            p.motion(SERVER, 150, 180),
            Some(InputEvent::MouseMotion { dx: 50, dy: -20 })
        ));
        assert!(matches!(
            p.motion(SERVER, 140, 190),
            Some(InputEvent::MouseMotion { dx: -10, dy: 10 })
        ));
    }

    #[test]
    fn a_mode_change_keeps_the_reference_point() {
        let mut p = PointerTranslator::new();
        p.motion(CLIENT, 500, 500);
        assert!(matches!(
            p.motion(SERVER, 600, 500),
            Some(InputEvent::MouseMotion { dx: 100, dy: 0 })
        ));
    }

    #[test]
    fn a_server_mode_button_elsewhere_is_preceded_by_the_delta_to_it() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 100, 100);
        let events = p.button(SERVER, mouse_buttons::LEFT, true, Some((130, 90)));
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            InputEvent::MouseMotion { dx: 30, dy: -10 }
        ));
        assert!(matches!(
            events[1],
            InputEvent::MouseDown {
                button: mouse_buttons::LEFT,
                x: 130,
                y: 90
            }
        ));
        assert_eq!(p.last_pos(), Some((130, 90)));
    }

    #[test]
    fn a_server_mode_button_in_place_sends_only_the_button() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 100, 100);
        let events = p.button(SERVER, mouse_buttons::LEFT, false, Some((100, 100)));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::MouseUp {
                button: mouse_buttons::LEFT,
                x: 100,
                y: 100
            }
        ));
    }

    #[test]
    fn a_server_mode_button_before_any_move_sends_only_the_button() {
        let mut p = PointerTranslator::new();
        let events = p.button(SERVER, mouse_buttons::LEFT, true, Some((40, 50)));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::MouseDown { x: 40, y: 50, .. }
        ));
        assert_eq!(p.last_pos(), Some((40, 50)));
    }

    #[test]
    fn a_client_mode_button_elsewhere_has_no_catch_up() {
        let mut p = PointerTranslator::new();
        p.motion(CLIENT, 100, 100);
        let events = p.button(CLIENT, mouse_buttons::RIGHT, true, Some((130, 90)));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::MouseDown {
                button: mouse_buttons::RIGHT,
                x: 130,
                y: 90
            }
        ));
        // Recorded all the same, so a later move is measured from it.
        assert_eq!(p.last_pos(), Some((130, 90)));
        assert!(p.motion(CLIENT, 130, 90).is_none());
    }

    #[test]
    fn a_button_without_a_position_uses_the_last_one() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 70, 80);
        let events = p.button(SERVER, mouse_buttons::MIDDLE, true, None);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::MouseDown {
                button: mouse_buttons::MIDDLE,
                x: 70,
                y: 80
            }
        ));
    }

    #[test]
    fn a_button_without_any_position_lands_at_the_origin() {
        let mut p = PointerTranslator::new();
        let events = p.button(CLIENT, mouse_buttons::LEFT, false, None);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::MouseUp { x: 0, y: 0, .. }));
        assert_eq!(p.last_pos(), None);
    }

    #[test]
    fn wheel_up_sends_button_four_pairs_at_the_last_position() {
        let mut p = PointerTranslator::new();
        p.motion(CLIENT, 5, 6);
        let events = p.wheel(2);
        assert_eq!(events.len(), 4);
        for pair in events.chunks(2) {
            assert!(matches!(
                pair[0],
                InputEvent::MouseDown {
                    button: mouse_buttons::UP,
                    x: 5,
                    y: 6
                }
            ));
            assert!(matches!(
                pair[1],
                InputEvent::MouseUp {
                    button: mouse_buttons::UP,
                    x: 5,
                    y: 6
                }
            ));
        }
    }

    #[test]
    fn wheel_down_sends_button_five_pairs() {
        let p = PointerTranslator::new();
        let events = p.wheel(-1);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            InputEvent::MouseDown {
                button: mouse_buttons::DOWN,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            InputEvent::MouseUp {
                button: mouse_buttons::DOWN,
                ..
            }
        ));
    }

    #[test]
    fn no_wheel_clicks_send_nothing() {
        assert!(PointerTranslator::new().wheel(0).is_empty());
    }

    #[test]
    fn reset_forgets_the_reference_point() {
        let mut p = PointerTranslator::new();
        p.motion(SERVER, 100, 100);
        p.reset();
        assert_eq!(p.last_pos(), None);
        // A fresh first move, not a delta from before the reset.
        assert!(p.motion(SERVER, 300, 300).is_none());
    }
}
