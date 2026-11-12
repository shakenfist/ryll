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

use std::collections::{BTreeMap, HashMap};

use shakenfist_spice_protocol::warn_once;
use tracing::{debug, info};

use crate::channels::ChannelEvent;
use crate::display::{DisplaySurface, MAX_SURFACE_PIXELS};

/// Surface ids a mirror accepts are below this.
///
/// spice-server keeps its surfaces in a fixed array of `NUM_SURFACES`
/// (1024, `server/display-limits.h`) and asserts the id is below it
/// before sending `SURFACE_CREATE`, so a larger id never comes from a
/// real server. Refusing it bounds how many surfaces one display
/// channel can make a mirror hold.
pub const MAX_SURFACE_ID: u32 = 1024;

/// The most pixels a mirror holds across all its surfaces.
///
/// Twice the per-surface cap, [`MAX_SURFACE_PIXELS`]: room for the
/// largest primary surface plus as much again in other heads and
/// off-screen surfaces, which a QXL guest backs with its video memory
/// (64 MiB by default). Without it, fresh surface ids at the
/// per-surface cap would each commit another 256 MiB.
pub const MAX_TOTAL_SURFACE_PIXELS: usize = 2 * MAX_SURFACE_PIXELS;

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
    /// `SurfaceCreated`, or an `ImageReady` that would have
    /// auto-created a surface, was refused: its id is not below
    /// [`MAX_SURFACE_ID`], or it would take the mirror past
    /// [`MAX_TOTAL_SURFACE_PIXELS`]. No surface is held at `key`
    /// afterwards; a refused create also drops the surface it would
    /// have replaced, which the server no longer has.
    Refused { key: (u8, u32) },
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
                // The surface this create replaces is gone server-side
                // whether or not the new one is admitted.
                self.surfaces.remove(&key);
                if !self.admits(key, *width, *height) {
                    return DrawOutcome::Refused { key };
                }
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
                if !self.surfaces.contains_key(&key)
                    && !self.admits(
                        key,
                        left.saturating_add(*width),
                        top.saturating_add(*height),
                    )
                {
                    return DrawOutcome::Refused { key };
                }
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

    /// Whether a new surface at `key`, asked for as `width` x
    /// `height`, may be created: its id must be below
    /// [`MAX_SURFACE_ID`], and its clamped size must fit within
    /// [`MAX_TOTAL_SURFACE_PIXELS`] beside the surfaces already held.
    /// `key` must not be held already. Logs, once, why not.
    fn admits(&self, key: (u8, u32), width: u32, height: u32) -> bool {
        let held: usize = self
            .surfaces
            .values()
            .map(|s| s.width as usize * s.height as usize)
            .sum();
        match refusal(held, key, width, height) {
            None => true,
            Some(Refusal::IdOutOfRange) => {
                warn_once!(
                    "display:decode_failure:surface:id_out_of_range",
                    "surface_mirror: refusing surface {}:{}: ids must be below {}",
                    key.0,
                    key.1,
                    MAX_SURFACE_ID
                );
                false
            }
            Some(Refusal::OverBudget) => {
                let (w, h) = DisplaySurface::clamped_size(width, height);
                warn_once!(
                    "display:decode_failure:surface:budget_exceeded",
                    "surface_mirror: refusing surface {}:{} ({}x{}): {} pixels already held, \
                     and the limit across all surfaces is {}",
                    key.0,
                    key.1,
                    w,
                    h,
                    held,
                    MAX_TOTAL_SURFACE_PIXELS
                );
                false
            }
        }
    }

    /// Bring the set of surfaces into line with `ledger` after this
    /// mirror missed events.
    ///
    /// A surface the ledger does not hold is dropped, and one it holds
    /// that is missing here, or held at another size, is created afresh
    /// through [`apply_event`](Self::apply_event), so the usual limits
    /// apply. A surface already held at the ledger's size keeps its
    /// pixels. Every surface is then marked fully damaged, since the
    /// draws that were missed are not restored. Events the mirror goes
    /// on to receive may repeat some the ledger already reflects: the
    /// broadcast channel still holds a suffix of the stream the mirror
    /// has just been brought past. Replaying it can briefly re-add or
    /// re-drop a surface, but the suffix is in order and the ledger
    /// reflects at least its end, so the mirror ends with the ledger's
    /// surfaces.
    pub fn resync(&mut self, ledger: &SurfaceLedger) {
        self.surfaces
            .retain(|key, _| ledger.surfaces.contains_key(key));
        for (&(display_channel_id, surface_id), &(width, height)) in &ledger.surfaces {
            let current = self
                .surfaces
                .get(&(display_channel_id, surface_id))
                .map(|s| (s.width, s.height));
            if current != Some(DisplaySurface::clamped_size(width, height)) {
                self.apply_event(&ChannelEvent::SurfaceCreated {
                    display_channel_id,
                    surface_id,
                    width,
                    height,
                });
            }
        }
        self.mark_all_damaged();
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

    /// Mark every surface fully damaged and dirty.
    ///
    /// For a caller that knows the mirror missed events (its broadcast
    /// subscription lagged, say). The pixels have already diverged from
    /// the guest's, and the recorded damage is missing the draws that
    /// were never applied, so every consumer must resend each surface
    /// whole rather than trust either.
    pub fn mark_all_damaged(&mut self) {
        for surface in self.surfaces.values_mut() {
            surface.mark_all_damaged();
        }
    }
}

impl Default for SurfaceMirror {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a new surface is refused.
enum Refusal {
    /// Its id is not below [`MAX_SURFACE_ID`].
    IdOutOfRange,
    /// It would take the total past [`MAX_TOTAL_SURFACE_PIXELS`].
    OverBudget,
}

/// Whether a new surface at `key`, asked for as `width` x `height`, is
/// refused beside `held` pixels of other surfaces. The one rule both
/// [`SurfaceMirror`] and [`SurfaceLedger`] apply, so that they agree on
/// which surfaces exist.
fn refusal(held: usize, key: (u8, u32), width: u32, height: u32) -> Option<Refusal> {
    if key.1 >= MAX_SURFACE_ID {
        return Some(Refusal::IdOutOfRange);
    }
    let (w, h) = DisplaySurface::clamped_size(width, height);
    if held + w as usize * h as usize > MAX_TOTAL_SURFACE_PIXELS {
        return Some(Refusal::OverBudget);
    }
    None
}

/// Which surfaces exist and at what size, without their pixels.
///
/// For a task that sees every event when the mirror might not. The
/// event bus's forwarder drains the renderer's queue, which loses
/// nothing, and observes each event here before it broadcasts it; the
/// mirror's subscription can lag and miss a `SurfaceCreated` or
/// `SurfaceDestroyed`, and [`SurfaceMirror::resync`] then repairs the
/// surface list from this. It tracks creates, destroys and
/// `ImageReady`'s auto-create by the same rules as
/// [`SurfaceMirror::apply_event`], so it holds the surfaces a mirror
/// that missed nothing would hold.
///
/// It also forgets every surface at `SessionInitialized`, which
/// `apply_event` does not do. A frontend that keeps one bus across
/// reconnects (the RDP bridge) empties its mirror itself when a new
/// session begins, and the surfaces of the session that ended do not
/// exist on the server any more: a resync must not bring them back,
/// or let them crowd a new session's surfaces out of the pixel budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceLedger {
    /// Requested `(width, height)` by `(display_channel_id,
    /// surface_id)`. Ordered, so a resync admits surfaces in the same
    /// order every time.
    surfaces: BTreeMap<(u8, u32), (u32, u32)>,
}

impl SurfaceLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what `event` does to the set of surfaces, if anything.
    pub fn observe(&mut self, event: &ChannelEvent) {
        match event {
            // A new session starts with no surfaces. Its display
            // channels are opened only after the main channel has
            // handled INIT, so none of its surfaces precede this.
            ChannelEvent::SessionInitialized(_) => self.surfaces.clear(),
            ChannelEvent::SurfaceCreated {
                display_channel_id,
                surface_id,
                width,
                height,
            } => {
                let key = (*display_channel_id, *surface_id);
                self.surfaces.remove(&key);
                self.admit(key, *width, *height);
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
                ..
            } => {
                let key = (*display_channel_id, *surface_id);
                if !self.surfaces.contains_key(&key) {
                    self.admit(
                        key,
                        left.saturating_add(*width),
                        top.saturating_add(*height),
                    );
                }
            }
            _ => {}
        }
    }

    fn admit(&mut self, key: (u8, u32), width: u32, height: u32) {
        let held: usize = self
            .surfaces
            .values()
            .map(|&(w, h)| {
                let (w, h) = DisplaySurface::clamped_size(w, h);
                w as usize * h as usize
            })
            .sum();
        if refusal(held, key, width, height).is_none() {
            self.surfaces.insert(key, (width, height));
        }
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
    use crate::display::DamageRect;

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

    fn create(m: &mut SurfaceMirror, surface_id: u32, width: u32, height: u32) -> DrawOutcome {
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id,
            width,
            height,
        })
    }

    #[test]
    fn surface_id_at_or_above_the_server_limit_is_refused() {
        let mut m = SurfaceMirror::new();
        assert!(matches!(
            create(&mut m, MAX_SURFACE_ID - 1, 8, 8),
            DrawOutcome::Created { .. }
        ));
        assert_eq!(
            create(&mut m, MAX_SURFACE_ID, 8, 8),
            DrawOutcome::Refused {
                key: (0, MAX_SURFACE_ID)
            }
        );
        assert_eq!(m.surfaces.len(), 1);
    }

    #[test]
    fn draw_to_an_out_of_range_id_does_not_auto_create() {
        let mut m = SurfaceMirror::new();
        let outcome = m.apply_event(&ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: u32::MAX,
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            pixels: vec![0u8; 16],
            image_id: 0,
            produced_at_secs: 0.0,
        });
        assert_eq!(outcome, DrawOutcome::Refused { key: (0, u32::MAX) });
        assert!(m.surfaces.is_empty());
    }

    #[test]
    fn surfaces_past_the_total_pixel_budget_are_refused() {
        // Each create asks for 16384 x 16384, which clamps to the
        // per-surface pixel cap. Two of those fill the budget.
        let mut m = SurfaceMirror::new();
        let side = crate::display::MAX_SURFACE_DIMENSION;
        assert!(matches!(
            create(&mut m, 1, side, side),
            DrawOutcome::Created { .. }
        ));
        assert!(matches!(
            create(&mut m, 2, side, side),
            DrawOutcome::Created { .. }
        ));
        assert_eq!(
            create(&mut m, 3, side, side),
            DrawOutcome::Refused { key: (0, 3) }
        );
        // So is a draw that would auto-create one more.
        let outcome = m.apply_event(&ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 4,
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            pixels: vec![0u8; 4],
            image_id: 0,
            produced_at_secs: 0.0,
        });
        assert_eq!(outcome, DrawOutcome::Refused { key: (0, 4) });
        let held: usize = m
            .surfaces
            .values()
            .map(|s| s.width as usize * s.height as usize)
            .sum();
        assert_eq!(held, MAX_TOTAL_SURFACE_PIXELS);
        // Destroying one makes room again.
        m.apply_event(&ChannelEvent::SurfaceDestroyed {
            display_channel_id: 0,
            surface_id: 1,
        });
        assert!(matches!(
            create(&mut m, 3, side, side),
            DrawOutcome::Created { .. }
        ));
    }

    #[test]
    fn recreating_a_surface_does_not_count_the_one_it_replaces() {
        // A primary re-created at the per-surface cap, over and over,
        // replaces itself; it must not be refused for holding the
        // budget against itself.
        let mut m = SurfaceMirror::new();
        let side = crate::display::MAX_SURFACE_DIMENSION;
        assert!(matches!(
            create(&mut m, 1, side, side),
            DrawOutcome::Created { .. }
        ));
        for _ in 0..3 {
            assert!(matches!(
                create(&mut m, 0, side, side),
                DrawOutcome::Created { .. }
            ));
        }
        assert_eq!(m.surfaces.len(), 2);
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
    fn draws_through_the_mirror_record_damage() {
        let mut m = SurfaceMirror::new();
        created(&mut m, 4, 4);
        m.primary_surface_mut().expect("primary").take_damage();
        m.apply_event(&image_ready(1, 1, 2, 2));
        let s = m.primary_surface().expect("primary");
        assert_eq!(s.damage().bounding_box(), Some(DamageRect::new(1, 1, 3, 3)));
    }

    #[test]
    fn mark_all_damaged_covers_every_surface() {
        let mut m = SurfaceMirror::new();
        created(&mut m, 4, 4);
        m.apply_event(&ChannelEvent::SurfaceCreated {
            display_channel_id: 1,
            surface_id: 7,
            width: 8,
            height: 2,
        });
        for s in m.surfaces.values_mut() {
            s.take_damage();
            s.consume_dirty();
        }

        m.mark_all_damaged();

        let primary = &m.surfaces[&(0, 0)];
        assert_eq!(primary.damage().rects(), &[DamageRect::new(0, 0, 4, 4)]);
        assert!(primary.is_dirty());
        let other = &m.surfaces[&(1, 7)];
        assert_eq!(other.damage().rects(), &[DamageRect::new(0, 0, 8, 2)]);
        assert!(other.is_dirty());
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

    /// Each surface a mirror holds, by key, with its size.
    fn sizes(m: &SurfaceMirror) -> Vec<((u8, u32), (u32, u32))> {
        let mut sizes: Vec<_> = m
            .surfaces
            .iter()
            .map(|(&key, s)| (key, (s.width, s.height)))
            .collect();
        sizes.sort_unstable();
        sizes
    }

    fn draw_to(surface_id: u32, left: u32, top: u32) -> ChannelEvent {
        ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id,
            left,
            top,
            width: 1,
            height: 1,
            pixels: vec![0u8; 4],
            image_id: 0,
            produced_at_secs: 0.0,
        }
    }

    fn created_event(surface_id: u32, width: u32, height: u32) -> ChannelEvent {
        ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id,
            width,
            height,
        }
    }

    fn destroyed_event(surface_id: u32) -> ChannelEvent {
        ChannelEvent::SurfaceDestroyed {
            display_channel_id: 0,
            surface_id,
        }
    }

    // The ledger must hold exactly the surfaces a mirror that saw the
    // same events holds, or a resync from it would add or drop the
    // wrong ones (#651).
    #[test]
    fn a_ledger_holds_what_a_mirror_fed_the_same_events_holds() {
        let events = [
            created_event(0, 8, 8),
            created_event(MAX_SURFACE_ID, 8, 8),
            draw_to(2, 5, 3),
            draw_to(2, 9, 9),
            created_event(1, 4, 4),
            created_event(1, 6, 2),
            created_event(3, 2, 2),
            destroyed_event(3),
            destroyed_event(9),
            draw_to(0, 1, 1),
        ];
        let mut mirror = SurfaceMirror::new();
        let mut ledger = SurfaceLedger::new();
        for event in &events {
            mirror.apply_event(event);
            ledger.observe(event);
        }
        let mut resynced = SurfaceMirror::new();
        resynced.resync(&ledger);
        assert_eq!(
            sizes(&mirror),
            vec![((0, 0), (8, 8)), ((0, 1), (6, 2)), ((0, 2), (6, 4))]
        );
        assert_eq!(sizes(&resynced), sizes(&mirror));
    }

    // A reconnect on a bus that outlives the session must not leave
    // the ended session's surfaces in the ledger, or a resync would put
    // them back beside the new session's, or recreate a surface the
    // new session holds at another size at the old one.
    #[test]
    fn a_new_session_empties_the_ledger() {
        let mut ledger = SurfaceLedger::new();
        for event in [
            ChannelEvent::SessionInitialized(1),
            created_event(0, 8, 8),
            created_event(1, 4, 4),
            ChannelEvent::SessionInitialized(2),
            created_event(0, 6, 6),
        ] {
            ledger.observe(&event);
        }
        // What the RDP bridge's mirror holds: emptied when the second
        // session began, then fed that session's surface.
        let mut mirror = SurfaceMirror::new();
        mirror.apply_event(&created_event(0, 6, 6));
        mirror.resync(&ledger);
        assert_eq!(sizes(&mirror), vec![((0, 0), (6, 6))]);
    }

    // A resync drops what the ledger does not hold, creates what it
    // holds and the mirror lacks, recreates a surface held at the wrong
    // size, and leaves a surface already right alone, pixels and all.
    #[test]
    fn resync_changes_only_the_surfaces_that_differ() {
        let mut m = SurfaceMirror::new();
        m.apply_event(&created_event(0, 2, 2));
        m.apply_event(&ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 0,
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            pixels: vec![255u8; 16],
            image_id: 0,
            produced_at_secs: 0.0,
        });
        m.apply_event(&created_event(2, 2, 2));
        m.apply_event(&created_event(5, 2, 2));
        m.surfaces.get_mut(&(0, 0)).unwrap().take_damage();

        let mut ledger = SurfaceLedger::new();
        for event in [
            created_event(0, 2, 2),
            created_event(1, 3, 3),
            created_event(2, 4, 4),
        ] {
            ledger.observe(&event);
        }
        m.resync(&ledger);

        assert_eq!(
            sizes(&m),
            vec![((0, 0), (2, 2)), ((0, 1), (3, 3)), ((0, 2), (4, 4))]
        );
        let kept = &m.surfaces[&(0, 0)];
        assert_eq!(kept.pixels(), &[255u8; 16][..], "pixels kept");
        assert_eq!(kept.damage().rects(), &[DamageRect::new(0, 0, 2, 2)]);
    }
}
