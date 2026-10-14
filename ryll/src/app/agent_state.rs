//! What ryll tells the user about the guest's SPICE agent (#537).
//!
//! The agent carries clipboard sharing and the guest resize requests.
//! A server with no agent at all (a guest without spice-vdagent, or a
//! server that does not implement one) looks, from the main channel, just
//! like an agent that is not connected yet, so the user needs telling
//! which it is and what it costs.

use eframe::egui;

/// Notice for a server that reported no agent when the session started.
/// Fixed text, so repeats across reconnects coalesce (#429).
pub(super) const NO_AGENT_NOTICE: &str =
    "The server has no guest agent: the clipboard is not shared \
     with the guest, and the guest display does not follow the window size";

/// Notice for an agent that has connected.
pub(super) const AGENT_CONNECTED_NOTICE: &str = "Guest agent connected";

/// Notice for an agent that was connected and has gone away.
pub(super) const AGENT_LOST_NOTICE: &str = "Guest agent disconnected";

/// Hint for a macOS paste chord (Cmd+V) with no agent, when
/// paste-as-keystrokes is enabled.
pub(super) const PASTE_HINT_KEYSTROKES: &str = "Cmd+V cannot paste into the guest without a \
     guest agent; Ctrl+Alt+V types the host clipboard into the guest instead";

/// Hint for a macOS paste chord (Cmd+V) with no agent, when
/// paste-as-keystrokes is not enabled.
pub(super) const PASTE_HINT_NO_KEYSTROKES: &str = "Cmd+V cannot paste into the guest without \
     a guest agent; run ryll with --enable-paste-as-keystrokes to type the host clipboard into \
     the guest with Ctrl+Alt+V instead";

/// The guest agent as this session has seen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum AgentState {
    /// The main channel has not reported yet: not connected, or still
    /// connecting.
    #[default]
    Unknown,
    /// The server reported no agent, and none has connected since.
    Absent,
    /// The agent is connected.
    Connected,
    /// The agent was connected in this session and has gone away.
    Lost,
}

impl AgentState {
    /// Fold one report from the main channel (its `MAIN_INIT`, then each
    /// `AGENT_CONNECTED` or `AGENT_DISCONNECTED`) into the state. Returns
    /// the new state and the notice to raise, if the report changed
    /// anything the user should hear about.
    pub(super) fn on_report(self, connected: bool) -> (Self, Option<&'static str>) {
        match (self, connected) {
            (Self::Connected, true) => (Self::Connected, None),
            (_, true) => (Self::Connected, Some(AGENT_CONNECTED_NOTICE)),
            (Self::Unknown, false) => (Self::Absent, Some(NO_AGENT_NOTICE)),
            (Self::Connected, false) => (Self::Lost, Some(AGENT_LOST_NOTICE)),
            (state @ (Self::Absent | Self::Lost), false) => (state, None),
        }
    }

    pub(super) fn connected(self) -> bool {
        self == Self::Connected
    }

    /// Colour of the status bar's agent indicator.
    pub(super) fn indicator_colour(self) -> egui::Color32 {
        match self {
            Self::Unknown | Self::Absent => egui::Color32::from_rgb(120, 120, 120),
            Self::Connected => egui::Color32::from_rgb(60, 180, 60),
            Self::Lost => egui::Color32::from_rgb(220, 160, 60),
        }
    }

    /// Tooltip of the status bar's agent indicator.
    pub(super) fn tooltip(self) -> &'static str {
        match self {
            Self::Unknown => "Guest agent: not known until the session connects",
            Self::Absent => {
                "Guest agent: none. The server has no SPICE agent, so the clipboard is not \
                 shared with the guest and the guest display does not follow the window size."
            }
            Self::Connected => {
                "Guest agent: connected. It shares the clipboard between host and guest, and \
                 resizes the guest display to fit the window."
            }
            Self::Lost => {
                "Guest agent: disconnected. Clipboard sharing and guest resizing stop until it \
                 connects again."
            }
        }
    }
}

/// Whether this frame holds a macOS paste chord (Cmd+V). egui-winit
/// reports its press as `Event::Paste`, and ryll never forwards Cmd, so
/// without an agent the chord does nothing at all in the guest.
pub(super) fn mac_paste_chord(modifiers: egui::Modifiers, events: &[egui::Event]) -> bool {
    modifiers.mac_cmd
        && events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(_)))
}

/// The hint for a paste chord with no agent.
pub(super) fn paste_hint(enable_paste: bool) -> &'static str {
    if enable_paste {
        PASTE_HINT_KEYSTROKES
    } else {
        PASTE_HINT_NO_KEYSTROKES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_agent_at_connect_is_not_a_disconnect() {
        assert_eq!(
            AgentState::Unknown.on_report(false),
            (AgentState::Absent, Some(NO_AGENT_NOTICE))
        );
    }

    #[test]
    fn agent_at_connect_is_connected() {
        assert_eq!(
            AgentState::Unknown.on_report(true),
            (AgentState::Connected, Some(AGENT_CONNECTED_NOTICE))
        );
    }

    #[test]
    fn agent_arriving_later_is_connected() {
        assert_eq!(
            AgentState::Absent.on_report(true),
            (AgentState::Connected, Some(AGENT_CONNECTED_NOTICE))
        );
        assert_eq!(
            AgentState::Lost.on_report(true),
            (AgentState::Connected, Some(AGENT_CONNECTED_NOTICE))
        );
    }

    #[test]
    fn agent_going_away_is_a_disconnect() {
        assert_eq!(
            AgentState::Connected.on_report(false),
            (AgentState::Lost, Some(AGENT_LOST_NOTICE))
        );
    }

    #[test]
    fn repeated_reports_are_quiet() {
        assert_eq!(
            AgentState::Connected.on_report(true),
            (AgentState::Connected, None)
        );
        assert_eq!(
            AgentState::Absent.on_report(false),
            (AgentState::Absent, None)
        );
        assert_eq!(AgentState::Lost.on_report(false), (AgentState::Lost, None));
    }

    #[test]
    fn only_connected_counts_as_connected() {
        assert!(AgentState::Connected.connected());
        for state in [AgentState::Unknown, AgentState::Absent, AgentState::Lost] {
            assert!(!state.connected());
        }
    }

    fn paste() -> egui::Event {
        egui::Event::Paste("host clipboard".to_string())
    }

    #[test]
    fn cmd_v_is_a_mac_paste_chord() {
        assert!(mac_paste_chord(egui::Modifiers::MAC_CMD, &[paste()]));
    }

    #[test]
    fn ctrl_v_is_not_a_mac_paste_chord() {
        // Off macOS Ctrl+V reaches the guest as Ctrl+V, so needs no hint.
        assert!(!mac_paste_chord(egui::Modifiers::CTRL, &[paste()]));
        assert!(!mac_paste_chord(
            egui::Modifiers::CTRL | egui::Modifiers::COMMAND,
            &[paste()]
        ));
    }

    #[test]
    fn cmd_without_paste_is_not_a_paste_chord() {
        let copy = [egui::Event::Copy];
        assert!(!mac_paste_chord(egui::Modifiers::MAC_CMD, &copy));
        assert!(!mac_paste_chord(egui::Modifiers::MAC_CMD, &[]));
    }

    #[test]
    fn paste_hint_matches_paste_as_keystrokes() {
        assert!(paste_hint(true).contains("Ctrl+Alt+V"));
        assert!(!paste_hint(true).contains("--enable-paste-as-keystrokes"));
        assert!(paste_hint(false).contains("--enable-paste-as-keystrokes"));
    }
}
