//! Surface mirror: applies SPICE display [`ChannelEvent`]s to a
//! [`HashMap`] of [`DisplaySurface`].
//!
//! [`SurfaceMirror::apply_event`] is the single draw-op dispatch
//! every mode uses: the GUI, the headless control socket, and the
//! `--web` mode's `RealFrameSource` all feed display events through
//! it, so the three cannot drift apart. It reports what it did as a
//! [`DrawOutcome`] so a frontend can layer its own reactions (auto-fit
//! resizes, texture invalidation, frame counters) on top. Cursor and
//! audio events are deliberately not handled here — those have
//! separate observers (cursor relay, audio sink).
//!
//! The mirror lives in the renderer crate (rather than `ryll/`)
//! because [`crate::encoder::RealFrameSource`] reads from it and
//! the renderer cannot back-depend on `ryll`. Lifting the mirror
//! up here keeps the crate boundary clean.

use std::collections::HashMap;

use tracing::{debug, info};

use crate::channels::ChannelEvent;
use crate::display::DisplaySurface;

/// What [`SurfaceMirror::apply_event`] did with one event.
///
/// `key` is `(display_channel_id, surface_id)`. Sizes are the ones
/// the server asked for, before [`DisplaySurface::new`] clamps them,
/// so a frontend can tell an oversized request from a real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawOutcome {
    /// The event is not a display draw op; the mirror ignored it.
    NotDisplay,
    /// `SurfaceCreated` inserted a surface, replacing any surface
    /// already at `key`.
    Created {
        key: (u8, u32),
        width: u32,
        height: u32,
    },
    /// `ImageReady` targeted an unknown surface, so one was created
    /// big enough to hold the draw and then drawn into.
    AutoCreated {
        key: (u8, u32),
        width: u32,
        height: u32,
    },
    /// `SurfaceDestroyed` removed the surface at `key`.
    Destroyed { key: (u8, u32) },
    /// A draw op landed on the existing surface at `key`.
    Drawn { key: (u8, u32) },
    /// A draw op, or a `SurfaceDestroyed`, named a surface the
    /// mirror does not hold; nothing changed.
    UnknownSurface { key: (u8, u32) },
}

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
    /// [`DisplaySurface`]; everything else is ignored and reports
    /// [`DrawOutcome::NotDisplay`]. This is the one dispatch every
    /// mode shares; mode-specific reactions (egui repaint hints,
    /// resolution-change notifications, frame stats) belong to the
    /// caller, driven by the returned [`DrawOutcome`].
    pub fn apply_event(&mut self, event: &ChannelEvent) -> DrawOutcome {
        match event {
            ChannelEvent::SurfaceCreated {
                display_channel_id,
                surface_id,
                width,
                height,
            } => {
                info!(
                    "surface_mirror: surface {}:{} created: {}x{}",
                    display_channel_id, surface_id, width, height
                );
                let key = (*display_channel_id, *surface_id);
                self.surfaces
                    .insert(key, DisplaySurface::new(*surface_id, *width, *height));
                DrawOutcome::Created {
                    key,
                    width: *width,
                    height: *height,
                }
            }

            ChannelEvent::SurfaceDestroyed {
                display_channel_id,
                surface_id,
            } => {
                let key = (*display_channel_id, *surface_id);
                if self.surfaces.remove(&key).is_some() {
                    info!(
                        "surface_mirror: surface {}:{} destroyed",
                        display_channel_id, surface_id
                    );
                    DrawOutcome::Destroyed { key }
                } else {
                    debug!(
                        "surface_mirror: SurfaceDestroyed on unknown surface {}",
                        surface_id
                    );
                    DrawOutcome::UnknownSurface { key }
                }
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
                // Sizes saturate so a hostile left + width cannot overflow.
                let key = (*display_channel_id, *surface_id);
                let mut outcome = DrawOutcome::Drawn { key };
                let entry = self.surfaces.entry(key).or_insert_with(|| {
                    let surf_w = left.saturating_add(*width);
                    let surf_h = top.saturating_add(*height);
                    info!(
                        "surface_mirror: auto-creating surface {} ({}x{}) from draw at ({},{})+{}x{}",
                        surface_id, surf_w, surf_h, left, top, width, height
                    );
                    outcome = DrawOutcome::AutoCreated {
                        key,
                        width: surf_w,
                        height: surf_h,
                    };
                    DisplaySurface::new(*surface_id, surf_w, surf_h)
                });
                entry.blit(*left, *top, *width, *height, pixels);
                debug!(
                    "surface_mirror: blit surface={}, pos=({},{}), size={}x{}",
                    surface_id, left, top, width, height
                );
                outcome
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
                let key = (*display_channel_id, *surface_id);
                if let Some(s) = self.surfaces.get_mut(&key) {
                    s.blit_chroma(*left, *top, *width, *height, pixels, *chroma_rgba);
                    DrawOutcome::Drawn { key }
                } else {
                    debug!(
                        "surface_mirror: ImageReadyChroma on unknown surface {}",
                        surface_id
                    );
                    DrawOutcome::UnknownSurface { key }
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
                let key = (*display_channel_id, *surface_id);
                if let Some(s) = self.surfaces.get_mut(&key) {
                    s.blit_alpha(*left, *top, *width, *height, pixels, *alpha);
                    DrawOutcome::Drawn { key }
                } else {
                    debug!(
                        "surface_mirror: ImageReadyAlpha on unknown surface {}",
                        surface_id
                    );
                    DrawOutcome::UnknownSurface { key }
                }
            }

            ChannelEvent::FillRect {
                display_channel_id,
                surface_id,
                rect: (left, top, right, bottom),
                colour,
                clip,
                ..
            } => {
                let key = (*display_channel_id, *surface_id);
                if let Some(s) = self.surfaces.get_mut(&key) {
                    s.fill_rect(*left, *top, *right, *bottom, *colour, clip);
                    DrawOutcome::Drawn { key }
                } else {
                    debug!("surface_mirror: FillRect on unknown surface {}", surface_id);
                    DrawOutcome::UnknownSurface { key }
                }
            }

            ChannelEvent::CopyBits {
                display_channel_id,
                surface_id,
                src_x,
                src_y,
                dest_rect: (left, top, right, bottom),
                clip,
                ..
            } => {
                let key = (*display_channel_id, *surface_id);
                if let Some(s) = self.surfaces.get_mut(&key) {
                    s.copy_bits(*src_x, *src_y, *left, *top, *right, *bottom, clip);
                    DrawOutcome::Drawn { key }
                } else {
                    debug!("surface_mirror: CopyBits on unknown surface {}", surface_id);
                    DrawOutcome::UnknownSurface { key }
                }
            }

            ChannelEvent::Invert {
                display_channel_id,
                surface_id,
                rect: (left, top, right, bottom),
                clip,
                ..
            } => {
                let key = (*display_channel_id, *surface_id);
                if let Some(s) = self.surfaces.get_mut(&key) {
                    s.invert_rect(*left, *top, *right, *bottom, clip);
                    DrawOutcome::Drawn { key }
                } else {
                    debug!("surface_mirror: Invert on unknown surface {}", surface_id);
                    DrawOutcome::UnknownSurface { key }
                }
            }

            // All non-display events (cursor, audio, session
            // bookkeeping, USB/WebDAV state, etc.) are observed
            // elsewhere — see the cursor relay and audio sink.
            _ => DrawOutcome::NotDisplay,
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

impl SurfaceMirror {
    /// Construct a `SurfaceMirror` pre-populated with a single RGBA
    /// surface at key `(display_channel_id, surface_id)`, filled with
    /// the supplied pixel data.
    ///
    /// Intended for integration tests that need a ready-made mirror
    /// without driving the full `ChannelEvent` pipeline.  The caller
    /// supplies raw RGBA bytes (`width * height * 4`); if the slice is
    /// too short the surface pixels are zero-padded; if it is too long
    /// the excess is silently ignored (same semantics as `blit`).
    ///
    /// Panics if `width` or `height` is 0.
    pub fn with_test_surface(
        display_channel_id: u8,
        surface_id: u32,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Self {
        assert!(
            width > 0 && height > 0,
            "surface must have non-zero dimensions"
        );
        let mut mirror = Self::new();
        let mut surface = DisplaySurface::new(surface_id, width, height);
        // Blit the supplied pixels into the entire surface area.
        surface.blit(0, 0, width, height, pixels);
        mirror
            .surfaces
            .insert((display_channel_id, surface_id), surface);
        mirror
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
            produced_at_secs: 0.0,
        });
        let s = m.primary_surface().expect("primary");
        assert_eq!(&s.pixels()[0..4], &[200, 100, 50, 255]);
    }

    fn created(m: &mut SurfaceMirror, width: u32, height: u32) -> DrawOutcome {
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width,
            height,
        })
    }

    fn image_ready(left: u32, top: u32, width: u32, height: u32) -> ChannelEvent {
        ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 0,
            left,
            top,
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
            image_id: 0,
            produced_at_secs: 0.0,
        }
    }

    /// The five draw ops other than `ImageReady`, all aimed at `(0, 0)`.
    fn other_draw_ops() -> Vec<ChannelEvent> {
        vec![
            ChannelEvent::ImageReadyChroma {
                display_channel_id: 0,
                surface_id: 0,
                left: 0,
                top: 0,
                width: 1,
                height: 1,
                pixels: vec![0u8; 4],
                chroma_rgba: [0, 0, 0, 255],
                image_id: 0,
                produced_at_secs: 0.0,
            },
            ChannelEvent::ImageReadyAlpha {
                display_channel_id: 0,
                surface_id: 0,
                left: 0,
                top: 0,
                width: 1,
                height: 1,
                pixels: vec![0u8; 4],
                alpha: 128,
                image_id: 0,
                produced_at_secs: 0.0,
            },
            ChannelEvent::FillRect {
                display_channel_id: 0,
                surface_id: 0,
                rect: (0, 0, 1, 1),
                colour: [1, 2, 3, 255],
                clip: vec![],
                produced_at_secs: 0.0,
            },
            ChannelEvent::CopyBits {
                display_channel_id: 0,
                surface_id: 0,
                src_x: 0,
                src_y: 0,
                dest_rect: (1, 1, 2, 2),
                clip: vec![],
                produced_at_secs: 0.0,
            },
            ChannelEvent::Invert {
                display_channel_id: 0,
                surface_id: 0,
                rect: (0, 0, 1, 1),
                clip: vec![],
                produced_at_secs: 0.0,
            },
        ]
    }

    #[test]
    fn outcome_not_display() {
        let mut m = SurfaceMirror::new();
        let outcome = m.apply_event(&ChannelEvent::DisplayMark {
            produced_at_secs: 0.0,
        });
        assert_eq!(outcome, DrawOutcome::NotDisplay);
    }

    #[test]
    fn outcome_created() {
        let mut m = SurfaceMirror::new();
        assert_eq!(
            created(&mut m, 64, 32),
            DrawOutcome::Created {
                key: (0, 0),
                width: 64,
                height: 32
            }
        );
    }

    #[test]
    fn outcome_created_when_replacing_existing_surface() {
        let mut m = SurfaceMirror::new();
        created(&mut m, 8, 8);
        assert_eq!(
            created(&mut m, 16, 4),
            DrawOutcome::Created {
                key: (0, 0),
                width: 16,
                height: 4
            }
        );
        assert_eq!(m.primary_surface().expect("primary").size(), (16, 4));
    }

    #[test]
    fn outcome_auto_created() {
        let mut m = SurfaceMirror::new();
        assert_eq!(
            m.apply_event(&image_ready(2, 3, 4, 5)),
            DrawOutcome::AutoCreated {
                key: (0, 0),
                width: 6,
                height: 8
            }
        );
    }

    #[test]
    fn outcome_destroyed() {
        let mut m = SurfaceMirror::new();
        created(&mut m, 8, 8);
        let outcome = m.apply_event(&ChannelEvent::SurfaceDestroyed {
            display_channel_id: 0,
            surface_id: 0,
        });
        assert_eq!(outcome, DrawOutcome::Destroyed { key: (0, 0) });
    }

    #[test]
    fn outcome_destroy_of_absent_surface_is_unknown() {
        let mut m = SurfaceMirror::new();
        let outcome = m.apply_event(&ChannelEvent::SurfaceDestroyed {
            display_channel_id: 1,
            surface_id: 2,
        });
        assert_eq!(outcome, DrawOutcome::UnknownSurface { key: (1, 2) });
    }

    #[test]
    fn outcome_drawn_for_every_draw_op() {
        let mut m = SurfaceMirror::new();
        created(&mut m, 4, 4);
        assert_eq!(
            m.apply_event(&image_ready(0, 0, 1, 1)),
            DrawOutcome::Drawn { key: (0, 0) }
        );
        for event in other_draw_ops() {
            assert_eq!(
                m.apply_event(&event),
                DrawOutcome::Drawn { key: (0, 0) },
                "{}",
                event.kind()
            );
        }
    }

    #[test]
    fn outcome_unknown_surface_for_draw_ops() {
        let mut m = SurfaceMirror::new();
        for event in other_draw_ops() {
            assert_eq!(
                m.apply_event(&event),
                DrawOutcome::UnknownSurface { key: (0, 0) },
                "{}",
                event.kind()
            );
        }
        assert!(m.surfaces.is_empty());
    }

    #[test]
    fn image_ready_auto_create_saturates_horizontal_overflow() {
        // left + width overflows u32; the GUI's old unchecked add
        // panicked here in debug builds.
        let mut m = SurfaceMirror::new();
        let outcome = m.apply_event(&image_ready(u32::MAX - 1, 0, 4, 1));
        assert_eq!(
            outcome,
            DrawOutcome::AutoCreated {
                key: (0, 0),
                width: u32::MAX,
                height: 1
            }
        );
    }

    #[test]
    fn image_ready_auto_create_saturates_vertical_overflow() {
        let mut m = SurfaceMirror::new();
        let outcome = m.apply_event(&image_ready(0, u32::MAX - 1, 1, 4));
        assert_eq!(
            outcome,
            DrawOutcome::AutoCreated {
                key: (0, 0),
                width: 1,
                height: u32::MAX
            }
        );
    }
}
