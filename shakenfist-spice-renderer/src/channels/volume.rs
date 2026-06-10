//! Volume control state shared between the audio playback channel
//! and the GUI/control surfaces that adjust it.
//!
//! `VolumeControl` is intentionally tiny and audio-stack-free: it
//! only carries two atomics.  Keeping it out of `playback.rs`
//! means callers (`session::run_connection` takes an
//! `Arc<VolumeControl>` regardless of audio mode) can always
//! construct it, and the rest of the audio plumbing
//! (`PlaybackChannel`, cpal stream, opus decoder) gates cleanly
//! behind the `audio` feature.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

pub struct VolumeControl {
    volume: AtomicU8,
    muted: AtomicBool,
}

impl VolumeControl {
    pub fn new() -> Arc<Self> {
        Arc::new(VolumeControl {
            volume: AtomicU8::new(80),
            muted: AtomicBool::new(false),
        })
    }

    pub fn volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, v: u8) {
        self.volume.store(v.min(100), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn effective_volume(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.volume() as f32 / 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VolumeControl;

    #[test]
    fn volume_control_new_defaults() {
        let vc = VolumeControl::new();
        assert_eq!(vc.volume(), 80);
        assert!(!vc.muted());
    }

    #[test]
    fn volume_control_set_volume_clamps_to_100() {
        let vc = VolumeControl::new();
        vc.set_volume(150);
        assert_eq!(vc.volume(), 100);
    }

    #[test]
    fn volume_control_effective_volume_when_muted_is_zero() {
        let vc = VolumeControl::new();
        vc.set_muted(true);
        assert_eq!(vc.effective_volume(), 0.0);
    }

    #[test]
    fn volume_control_effective_volume_default() {
        let vc = VolumeControl::new();
        let ev = vc.effective_volume();
        assert!((ev - 0.8).abs() < 1e-6, "expected 0.8, got {}", ev);
    }

    #[test]
    fn volume_control_mute_unmute_preserves_volume() {
        let vc = VolumeControl::new();
        vc.set_volume(65);
        vc.set_muted(true);
        assert_eq!(vc.effective_volume(), 0.0);
        vc.set_muted(false);
        assert_eq!(vc.volume(), 65);
        let ev = vc.effective_volume();
        assert!((ev - 0.65).abs() < 1e-6, "expected 0.65, got {}", ev);
    }
}
