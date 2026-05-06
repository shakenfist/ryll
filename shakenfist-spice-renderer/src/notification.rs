//! Notification data types emitted by channels via
//! `ChannelEvent::Notification`.
//!
//! The store that holds these entries lives in the host (ryll's
//! `NotificationStore`). Channels emit `ChannelEvent::Notification`
//! events; the host's event drain pushes them into the store.
//! This decouples the channel layer from the GUI-facing store.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use shakenfist_spice_protocol::{ChannelType, NotifySeverity, SpiceVisibility};

/// Origin of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationSource {
    /// Protocol gap registered via `warn_once!`.
    Gap,
    /// Bug-report writer success/failure status.
    BugReport,
    /// SPICE_MSG_NOTIFY received on a channel.
    Spice { channel: ChannelType, what: u32 },
    /// Internally generated notification.
    Internal,
}

impl NotificationSource {
    /// Compact human label for the side panel.
    pub fn label(&self) -> String {
        match self {
            NotificationSource::Gap => "Gap".to_string(),
            NotificationSource::BugReport => "BugReport".to_string(),
            NotificationSource::Internal => "Internal".to_string(),
            NotificationSource::Spice { channel, .. } => {
                format!("SPICE/{}", channel.name())
            }
        }
    }
}

/// A single notification entry.
///
/// `id` is `0` until the host's `NotificationStore::push` stamps
/// it. The id is opaque to channel code — channels construct
/// fresh entries and emit them as events; the store assigns ids
/// when it folds or appends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationEntry {
    pub id: u64,
    pub when: SystemTime,
    pub severity: NotifySeverity,
    pub source: NotificationSource,
    pub message: String,
    pub count: u32,
    pub visibility: Option<SpiceVisibility>,
    pub read: bool,
}

impl NotificationEntry {
    /// Build a fresh entry. `id` is 0 until the store stamps it.
    pub fn new(
        severity: NotifySeverity,
        source: NotificationSource,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: 0,
            when: SystemTime::now(),
            severity,
            source,
            message: message.into(),
            count: 1,
            visibility: None,
            read: false,
        }
    }

    /// Builder-style setter for SPICE visibility.
    pub fn with_visibility(mut self, v: SpiceVisibility) -> Self {
        self.visibility = Some(v);
        self
    }
}
