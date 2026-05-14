//! Surface mirror: applies SPICE display [`ChannelEvent`]s to a
//! [`HashMap`] of [`DisplaySurface`].
//!
//! The mirror is the substrate the `--web` mode's `RealFrameSource`
//! reads from. The dispatch in [`SurfaceMirror::apply_event`]
//! mirrors the display-bearing arms of
//! `ryll/src/app.rs::process_events` so the web-side pixel store
//! stays in lock-step with what the GUI would draw. Cursor and
//! audio events are deliberately not handled here — those have
//! separate observers in web mode (cursor relay, audio sink).
//!
//! The mirror lives in the renderer crate (rather than `ryll/`)
//! because [`crate::encoder::RealFrameSource`] reads from it and
//! the renderer cannot back-depend on `ryll`. Lifting the mirror
//! up here keeps the crate boundary clean.

use std::collections::HashMap;

use crate::channels::ChannelEvent;
use crate::display::DisplaySurface;

/// Live pixel store rebuilt from a stream of [`ChannelEvent`]s.
///
/// Keyed by `(display_channel_id, surface_id)`. By SPICE
/// convention, `(0, 0)` is the primary surface; [`primary_key`]
/// falls back to any surface if the canonical primary key is
/// absent so a single-surface guest with non-zero IDs still
/// works.
///
/// [`primary_key`]: SurfaceMirror::primary_key
pub struct SurfaceMirror {
    pub surfaces: HashMap<(u8, u32), DisplaySurface>,
}

impl SurfaceMirror {
    /// Empty mirror with no surfaces. The first
    /// `SurfaceCreated` event (or the auto-create path on the
    /// first `ImageReady`) populates the primary entry.
    pub fn new() -> Self {
        Self {
            surfaces: HashMap::new(),
        }
    }

    /// Apply one [`ChannelEvent`] to the surface map.
    ///
    /// Display-bearing variants update the relevant
    /// [`DisplaySurface`]; everything else is ignored. Mirrors
    /// the dispatch in `ryll/src/app.rs::process_events` minus
    /// the GUI-only state mutations (egui repaint hints,
    /// resolution-change notifications, frame-time stats, etc.).
    pub fn apply_event(&mut self, event: &ChannelEvent) {
        match event {
            ChannelEvent::SurfaceCreated {
                display_channel_id,
                surface_id,
                width,
                height,
            } => {
                self.surfaces.insert(
                    (*display_channel_id, *surface_id),
                    DisplaySurface::new(*surface_id, *width, *height),
                );
            }

            ChannelEvent::SurfaceDestroyed {
                display_channel_id,
                surface_id,
            } => {
                self.surfaces.remove(&(*display_channel_id, *surface_id));
            }

            ChannelEvent::ImageReady {
                display_channel_id,
                surface_id,
                left,
                top,
                width,
                height,
                pixels,
                ..
            } => {
                // Auto-create surface if the server draws before sending
                // SURFACE_CREATE (QEMU does this for the primary surface).
                // Mirrors ryll/src/app.rs::process_events.
                let key = (*display_channel_id, *surface_id);
                let entry = self.surfaces.entry(key).or_insert_with(|| {
                    let surf_w = left.saturating_add(*width);
                    let surf_h = top.saturating_add(*height);
                    DisplaySurface::new(*surface_id, surf_w, surf_h)
                });
                entry.blit(*left, *top, *width, *height, pixels);
            }

            ChannelEvent::ImageReadyChroma {
                display_channel_id,
                surface_id,
                left,
                top,
                width,
                height,
                pixels,
                chroma_rgba,
                ..
            } => {
                if let Some(s) = self.surfaces.get_mut(&(*display_channel_id, *surface_id)) {
                    s.blit_chroma(*left, *top, *width, *height, pixels, *chroma_rgba);
                }
            }

            ChannelEvent::ImageReadyAlpha {
                display_channel_id,
                surface_id,
                left,
                top,
                width,
                height,
                pixels,
                alpha,
                ..
            } => {
                if let Some(s) = self.surfaces.get_mut(&(*display_channel_id, *surface_id)) {
                    s.blit_alpha(*left, *top, *width, *height, pixels, *alpha);
                }
            }

            ChannelEvent::FillRect {
                display_channel_id,
                surface_id,
                rect: (left, top, right, bottom),
                colour,
                clip,
            } => {
                if let Some(s) = self.surfaces.get_mut(&(*display_channel_id, *surface_id)) {
                    s.fill_rect(*left, *top, *right, *bottom, *colour, clip);
                }
            }

            ChannelEvent::CopyBits {
                display_channel_id,
                surface_id,
                src_x,
                src_y,
                dest_rect: (left, top, right, bottom),
                clip,
            } => {
                if let Some(s) = self.surfaces.get_mut(&(*display_channel_id, *surface_id)) {
                    s.copy_bits(*src_x, *src_y, *left, *top, *right, *bottom, clip);
                }
            }

            ChannelEvent::Invert {
                display_channel_id,
                surface_id,
                rect: (left, top, right, bottom),
                clip,
            } => {
                if let Some(s) = self.surfaces.get_mut(&(*display_channel_id, *surface_id)) {
                    s.invert_rect(*left, *top, *right, *bottom, clip);
                }
            }

            // All non-display events (cursor, audio, session
            // bookkeeping, USB/WebDAV state, etc.) are observed
            // elsewhere in web mode — see the cursor relay (5d)
            // and audio sink (5e). No-op here.
            _ => {}
        }
    }

    /// Key of the primary surface. SPICE convention is
    /// `(0, 0)`; if that's absent (rare, but possible during
    /// teardown) any one surface key is returned so callers
    /// can still find pixels to encode.
    pub fn primary_key(&self) -> Option<(u8, u32)> {
        if self.surfaces.contains_key(&(0, 0)) {
            Some((0, 0))
        } else {
            self.surfaces.keys().next().copied()
        }
    }

    /// Borrow the primary [`DisplaySurface`], if any. Returns
    /// `None` while the SPICE session is still initialising and
    /// no draw events have arrived yet.
    pub fn primary_surface(&self) -> Option<&DisplaySurface> {
        let key = self.primary_key()?;
        self.surfaces.get(&key)
    }

    /// Mutable borrow of the primary [`DisplaySurface`], used by
    /// [`crate::encoder::RealFrameSource`] to call
    /// [`DisplaySurface::consume_dirty`].
    pub fn primary_surface_mut(&mut self) -> Option<&mut DisplaySurface> {
        let key = self.primary_key()?;
        self.surfaces.get_mut(&key)
    }
}

impl Default for SurfaceMirror {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_created_inserts_entry() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 64,
            height: 32,
        });
        assert_eq!(m.surfaces.len(), 1);
        let s = m.primary_surface().expect("primary present");
        assert_eq!(s.size(), (64, 32));
    }

    #[test]
    fn surface_destroyed_removes_entry() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 16,
            height: 16,
        });
        assert_eq!(m.surfaces.len(), 1);
        m.apply_event(&ChannelEvent::SurfaceDestroyed {
            display_channel_id: 0,
            surface_id: 0,
        });
        assert!(m.surfaces.is_empty());
        assert!(m.primary_surface().is_none());
    }

    #[test]
    fn image_ready_blits_pixels() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 2,
            height: 2,
        });
        // 2x2 RGBA all-red image.
        let pixels: Vec<u8> = (0..4).flat_map(|_| [255u8, 0, 0, 255]).collect();
        m.apply_event(&ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 0,
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            pixels,
            image_id: 0,
            produced_at_secs: 0.0,
        });
        let s = m.primary_surface().expect("primary present");
        // First pixel should now be opaque red.
        assert_eq!(&s.pixels()[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn image_ready_auto_creates_surface() {
        // QEMU draws before SURFACE_CREATE for the primary surface.
        // The mirror must auto-create rather than drop the draw.
        let mut m = SurfaceMirror::new();
        let pixels: Vec<u8> = (0..16).flat_map(|_| [10u8, 20, 30, 255]).collect();
        m.apply_event(&ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 0,
            left: 0,
            top: 0,
            width: 4,
            height: 4,
            pixels,
            image_id: 0,
            produced_at_secs: 0.0,
        });
        assert_eq!(m.surfaces.len(), 1);
        let s = m.primary_surface().expect("primary auto-created");
        assert_eq!(s.size(), (4, 4));
        assert_eq!(&s.pixels()[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn primary_key_falls_back_when_zero_zero_absent() {
        let mut m = SurfaceMirror::new();
        // Insert only a non-(0,0) entry.
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 1,
            surface_id: 7,
            width: 8,
            height: 8,
        });
        let key = m.primary_key().expect("some key");
        assert_eq!(key, (1, 7));
        assert!(m.primary_surface().is_some());
    }

    #[test]
    fn primary_key_prefers_zero_zero() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 1,
            surface_id: 7,
            width: 8,
            height: 8,
        });
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 16,
            height: 16,
        });
        assert_eq!(m.primary_key(), Some((0, 0)));
        let s = m.primary_surface().expect("primary");
        assert_eq!(s.size(), (16, 16));
    }

    #[test]
    fn non_display_event_is_noop() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SessionInitialized(42));
        m.apply_event(&ChannelEvent::DisplayMark {
            produced_at_secs: 0.0,
        });
        m.apply_event(&ChannelEvent::CursorPosition {
            x: 10,
            y: 20,
            visible: true,
        });
        assert!(m.surfaces.is_empty());
    }

    #[test]
    fn fill_rect_paints_into_surface() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width: 4,
            height: 4,
        });
        m.apply_event(&ChannelEvent::FillRect {
            display_channel_id: 0,
            surface_id: 0,
            rect: (0, 0, 2, 2),
            colour: [200, 100, 50, 255],
            clip: vec![],
        });
        let s = m.primary_surface().expect("primary");
        assert_eq!(&s.pixels()[0..4], &[200, 100, 50, 255]);
    }
}
