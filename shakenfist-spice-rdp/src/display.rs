//! The RDP display handler: serves the session's primary surface to
//! the RDP client, as full-surface bitmaps after connect and after a
//! resize, and as damaged rectangles read from the surface mirror
//! otherwise.
//!
//! The handler reads a framebuffer rather than queueing bitmaps. The
//! server asks for the next update only once it has encoded and
//! written the previous one, so while a client is behind, damage
//! accumulates in the mirror and the next take reads the pixels as
//! they are by then. Pixels a later draw has already replaced are
//! never sent, and nothing grows without bound while the client
//! catches up.

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use shakenfist_spice_renderer::{DamageRect, DisplaySurface, SurfaceMirror};
use tokio::time::{Interval, MissedTickBehavior};
use tracing::debug;

use crate::SessionSlot;

/// How often the update stream looks at the mirror for new damage.
///
/// The mirror offers no wake-up when it applies a draw, and a
/// subscription to the session's event bus cannot be ordered after
/// the mirror has applied the event, so the stream polls. 16 ms is
/// one frame at 60 Hz: short enough that a poll adds no visible
/// latency, long enough that an idle session costs next to nothing.
const DAMAGE_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// Bytes per pixel of the surface mirror's RGBA pixels, which are
/// sent to the client as they are, in [`PixelFormat::RgbA32`].
const BYTES_PER_PIXEL: usize = 4;

/// The RDP display handler for the session in a [`SessionSlot`].
///
/// Reports the session's primary surface size as the desktop size and
/// hands the server an update stream that reads the surface mirror's
/// damage. Before a session has been chosen, or before its first
/// surface exists, the desktop is `fallback` in size; the update
/// stream corrects that with a resize once a surface appears.
pub struct MirrorDisplay {
    slot: SessionSlot,
    fallback: DesktopSize,
    /// The size [`RdpServerDisplay::size`] last reported. The server
    /// builds its encoder with that size immediately before it asks
    /// for an update stream, so a new stream starts from it.
    reported: Option<DesktopSize>,
}

impl MirrorDisplay {
    /// A display handler for whatever session `slot` holds, reporting
    /// `fallback` as the desktop size while there is no surface to
    /// measure.
    pub fn new(slot: SessionSlot, fallback: DesktopSize) -> Self {
        Self {
            slot,
            fallback,
            reported: None,
        }
    }

    /// The primary surface's size, or `fallback` when there is no
    /// session, no surface, or a surface with no pixels.
    async fn current_size(&self) -> DesktopSize {
        let Some(session) = self.slot.current() else {
            return self.fallback;
        };
        let mirror = session.surface_mirror.lock().await;
        mirror
            .primary_surface()
            .and_then(surface_desktop_size)
            .unwrap_or(self.fallback)
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MirrorDisplay {
    async fn size(&mut self) -> DesktopSize {
        let size = self.current_size().await;
        self.reported = Some(size);
        size
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let announced = match self.reported {
            Some(size) => size,
            None => self.size().await,
        };
        Ok(Box::new(MirrorUpdates::new(self.slot.clone(), announced)))
    }
}

/// One connection's stream of display updates, read from the mirror.
///
/// The server builds a new stream for every connection, and again
/// after every resize (a resize deactivates and reactivates the
/// connection), so a stream starts unseeded and sends the whole
/// surface first.
struct MirrorUpdates {
    slot: SessionSlot,
    /// Updates taken from the mirror and not yet handed to the server.
    pending: VecDeque<DisplayUpdate>,
    /// The desktop size the server's encoder currently has: the size
    /// announced when the stream was built, or the last resize sent.
    sent_size: DesktopSize,
    /// Whether the client has been sent the whole surface at
    /// `sent_size`. IronRDP seeds its framebuffer only from a bitmap
    /// covering the whole desktop, and without one it re-encodes every
    /// later bitmap in full instead of diffing it, so the first bitmap
    /// of a stream, and the first after a resize, must be the whole
    /// surface.
    seeded: bool,
    interval: Interval,
}

impl MirrorUpdates {
    fn new(slot: SessionSlot, announced: DesktopSize) -> Self {
        let mut interval = tokio::time::interval(DAMAGE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            slot,
            pending: VecDeque::new(),
            sent_size: announced,
            seeded: false,
            interval,
        }
    }

    /// Look at the mirror once and queue whatever the client needs:
    /// a resize and the whole surface if the surface changed size, the
    /// whole surface if the client has not had it yet, and otherwise
    /// one bitmap per damaged rectangle.
    ///
    /// Synchronous on purpose: everything taken from the mirror is in
    /// `pending` before this returns.
    fn poll_mirror(&mut self, mirror: &mut SurfaceMirror) {
        let Some(surface) = mirror.primary_surface_mut() else {
            // The surface has gone (the guest is reconfiguring its
            // display, or the session is ending). Whatever surface
            // comes next is sent whole.
            self.seeded = false;
            return;
        };
        let Some(size) = surface_desktop_size(surface) else {
            self.seeded = false;
            return;
        };

        if size != self.sent_size {
            debug!(
                "rdp display: surface is {}x{}, client has {}x{}; resizing",
                size.width, size.height, self.sent_size.width, self.sent_size.height
            );
            self.pending.push_back(DisplayUpdate::Resize(size));
            self.sent_size = size;
            self.seeded = false;
        }

        if !self.seeded {
            // The whole surface is about to be sent, so the damage
            // recorded so far is already covered.
            surface.take_damage();
            if let Some(bitmap) = full_bitmap(surface, size) {
                self.pending.push_back(DisplayUpdate::Bitmap(bitmap));
                self.seeded = true;
            }
            return;
        }

        let damage = surface.take_damage();
        self.pending
            .extend(damage_bitmaps(surface, size, damage.rects()).map(DisplayUpdate::Bitmap));
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MirrorUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // The server races this future against its other tasks in a
        // `tokio::select!` and drops it whenever another branch wins,
        // so it must lose nothing when dropped at an await. The only
        // awaits are the tick and the mirror lock, both before any
        // damage is taken; the take and the queueing happen together
        // in `poll_mirror`, with no await between them, and what is
        // taken stays in `self.pending` for the next call. The mirror
        // lock is held only for that synchronous step, so the SPICE
        // side is never kept waiting on an encode or a network write.
        //
        // The loop never ends on its own: the server treats `None` as
        // the end of the display stream and disconnects the client, so
        // while there is nothing to send this keeps waiting.
        loop {
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(update));
            }

            self.interval.tick().await;

            let Some(session) = self.slot.current() else {
                self.seeded = false;
                continue;
            };
            let mut mirror = session.surface_mirror.lock().await;
            self.poll_mirror(&mut mirror);
        }
    }
}

/// The RDP desktop size for `surface`, or `None` if it has no pixels.
///
/// RDP desktop sizes are 16 bits a side. Surfaces are capped well
/// below that, so the clamp here is a guard, not a path.
fn surface_desktop_size(surface: &DisplaySurface) -> Option<DesktopSize> {
    let (width, height) = surface.size();
    if width == 0 || height == 0 {
        return None;
    }
    let clamp = |side: u32| {
        u16::try_from(side).unwrap_or_else(|_| {
            shakenfist_spice_protocol::warn_once!(
                "rdp-display:surface-too-large",
                "rdp display: surface of {}x{} is larger than RDP allows; sending its top-left {}x{}",
                width,
                height,
                u16::MAX,
                u16::MAX
            );
            u16::MAX
        })
    };
    Some(DesktopSize {
        width: clamp(width),
        height: clamp(height),
    })
}

/// A bitmap of the whole desktop, from the surface's top-left corner.
fn full_bitmap(surface: &DisplaySurface, size: DesktopSize) -> Option<BitmapUpdate> {
    Some(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(size.width)?,
        height: NonZeroU16::new(size.height)?,
        format: PixelFormat::RgbA32,
        data: Bytes::copy_from_slice(surface.pixels()),
        stride: NonZeroUsize::new(row_bytes(surface))?,
    })
}

/// One bitmap per damaged rectangle, each clipped to the desktop.
///
/// The rows of the rectangles' bounding box are copied out of the
/// surface once, and each bitmap is a slice of that copy, so a burst
/// of rectangles costs one copy rather than one each.
fn damage_bitmaps(
    surface: &DisplaySurface,
    size: DesktopSize,
    rects: &[DamageRect],
) -> impl Iterator<Item = BitmapUpdate> {
    let desktop = DamageRect::new(0, 0, u32::from(size.width), u32::from(size.height));
    let clipped: Vec<DamageRect> = rects
        .iter()
        .map(|r| intersect(r, &desktop))
        .filter(|r| !r.is_empty())
        .collect();
    let bounds = clipped.iter().copied().reduce(|a, b| a.union(&b));
    let copy = bounds.and_then(|b| copy_rect(surface, &b).map(|bitmap| (b, bitmap)));

    clipped.into_iter().filter_map(move |rect| {
        let (bounds, bitmap) = copy.as_ref()?;
        // Every coordinate here is inside the desktop, so fits a u16.
        let x = u16::try_from(rect.left - bounds.left).ok()?;
        let y = u16::try_from(rect.top - bounds.top).ok()?;
        let width = NonZeroU16::new(u16::try_from(rect.width()).ok()?)?;
        let height = NonZeroU16::new(u16::try_from(rect.height()).ok()?)?;
        bitmap.sub(x, y, width, height)
    })
}

/// A bitmap of `rect`, copied out of the surface with a stride of the
/// rectangle's own width. `rect` must lie inside the surface and the
/// desktop.
fn copy_rect(surface: &DisplaySurface, rect: &DamageRect) -> Option<BitmapUpdate> {
    let src_stride = row_bytes(surface);
    let left = usize::try_from(rect.left).ok()? * BYTES_PER_PIXEL;
    let right = usize::try_from(rect.right).ok()? * BYTES_PER_PIXEL;
    let top = usize::try_from(rect.top).ok()?;
    let bottom = usize::try_from(rect.bottom).ok()?;
    let pixels = surface.pixels();

    let mut data = Vec::with_capacity((right - left) * (bottom - top));
    for row in top..bottom {
        let start = row * src_stride;
        data.extend_from_slice(pixels.get(start + left..start + right)?);
    }

    Some(BitmapUpdate {
        x: u16::try_from(rect.left).ok()?,
        y: u16::try_from(rect.top).ok()?,
        width: NonZeroU16::new(u16::try_from(rect.width()).ok()?)?,
        height: NonZeroU16::new(u16::try_from(rect.height()).ok()?)?,
        format: PixelFormat::RgbA32,
        data: Bytes::from(data),
        stride: NonZeroUsize::new(right - left)?,
    })
}

/// The part of `a` inside `b`; empty if they do not overlap.
fn intersect(a: &DamageRect, b: &DamageRect) -> DamageRect {
    DamageRect::new(
        a.left.max(b.left),
        a.top.max(b.top),
        a.right.min(b.right),
        a.bottom.min(b.bottom),
    )
}

/// Bytes in one row of the surface's pixels.
fn row_bytes(surface: &DisplaySurface) -> usize {
    // Surface sides are capped far below the point where this could
    // overflow or fail to convert.
    usize::try_from(surface.width).unwrap_or(0) * BYTES_PER_PIXEL
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use shakenfist_spice_renderer::ChannelEvent;
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;

    use super::*;
    use crate::SessionHandles;

    const FALLBACK: DesktopSize = DesktopSize {
        width: 1024,
        height: 768,
    };

    /// RGBA pixels for a `width` x `height` image, each byte differing
    /// from its neighbours so a misplaced row or column shows. `seed`
    /// tells two images of the same size apart.
    fn numbered_pixels(width: u32, height: u32, seed: u8) -> Vec<u8> {
        (0..(width * height * 4) as usize)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    }

    /// A slot holding a session whose mirror has one `width` x `height`
    /// primary surface of numbered pixels.
    fn session_with_surface(width: u32, height: u32) -> (SessionSlot, Arc<SessionHandles>) {
        let mirror = SurfaceMirror::with_test_surface(
            0,
            0,
            width,
            height,
            &numbered_pixels(width, height, 0),
        );
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = watch::channel(0);
        let (resize_tx, _) = mpsc::channel(1);
        let handles = Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            label: String::from("test"),
        });
        let slot = SessionSlot::new();
        slot.set(Arc::clone(&handles));
        (slot, handles)
    }

    /// A display over `slot`'s session and its update stream, built as
    /// the server builds them: size first, then updates.
    async fn connect(slot: &SessionSlot) -> (MirrorDisplay, Box<dyn RdpServerDisplayUpdates>) {
        let mut display = MirrorDisplay::new(slot.clone(), FALLBACK);
        display.size().await;
        let updates = display.updates().await.expect("updates");
        (display, updates)
    }

    async fn next(updates: &mut Box<dyn RdpServerDisplayUpdates>) -> DisplayUpdate {
        updates
            .next_update()
            .await
            .expect("next_update")
            .expect("an update")
    }

    async fn next_bitmap(updates: &mut Box<dyn RdpServerDisplayUpdates>) -> BitmapUpdate {
        match next(updates).await {
            DisplayUpdate::Bitmap(bitmap) => bitmap,
            other => panic!("expected a bitmap, got {other:?}"),
        }
    }

    async fn next_resize(updates: &mut Box<dyn RdpServerDisplayUpdates>) -> DesktopSize {
        match next(updates).await {
            DisplayUpdate::Resize(size) => size,
            other => panic!("expected a resize, got {other:?}"),
        }
    }

    /// The bitmap's pixels, row by row, without stride padding.
    fn bitmap_rows(bitmap: &BitmapUpdate) -> Vec<u8> {
        let row = usize::from(bitmap.width.get()) * 4;
        (0..usize::from(bitmap.height.get()))
            .flat_map(|y| {
                let start = y * bitmap.stride.get();
                bitmap.data[start..start + row].to_vec()
            })
            .collect()
    }

    /// The whole surface's pixels.
    async fn surface_pixels(handles: &SessionHandles) -> Vec<u8> {
        let mirror = handles.surface_mirror.lock().await;
        mirror.primary_surface().expect("surface").pixels().to_vec()
    }

    fn assert_geometry(bitmap: &BitmapUpdate, x: u16, y: u16, width: u16, height: u16) {
        assert_eq!(
            (bitmap.x, bitmap.y, bitmap.width.get(), bitmap.height.get()),
            (x, y, width, height)
        );
        assert_eq!(bitmap.format, PixelFormat::RgbA32);
    }

    async fn blit(handles: &SessionHandles, x: u32, y: u32, width: u32, height: u32, seed: u8) {
        let pixels = numbered_pixels(width, height, seed);
        let mut mirror = handles.surface_mirror.lock().await;
        mirror
            .primary_surface_mut()
            .expect("surface")
            .blit(x, y, width, height, &pixels);
    }

    #[tokio::test(start_paused = true)]
    async fn first_update_is_the_whole_surface() {
        let (slot, handles) = session_with_surface(8, 6);
        let (mut display, mut updates) = connect(&slot).await;
        assert_eq!(
            display.size().await,
            DesktopSize {
                width: 8,
                height: 6
            }
        );

        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
        assert_eq!(bitmap.stride.get(), 8 * 4);
        assert_eq!(bitmap_rows(&bitmap), surface_pixels(&handles).await);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(8, 6, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn blit_yields_one_bitmap_of_its_rectangle() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        blit(&handles, 2, 1, 3, 2, 100).await;
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 2, 1, 3, 2);
        assert_eq!(bitmap.stride.get(), 3 * 4);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(3, 2, 100));
    }

    #[tokio::test(start_paused = true)]
    async fn separated_rectangles_yield_two_bitmaps_in_one_take() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        blit(&handles, 0, 0, 2, 2, 50).await;
        blit(&handles, 5, 4, 2, 2, 150).await;
        let first = next_bitmap(&mut updates).await;
        // The second comes from the same take, without another tick.
        let taken_at = Instant::now();
        let second = next_bitmap(&mut updates).await;
        assert_eq!(Instant::now(), taken_at);

        let (top_left, bottom_right) = if first.x == 0 {
            (first, second)
        } else {
            (second, first)
        };
        // Both are slices of one copy of the bounding box, 7 pixels wide.
        assert_geometry(&top_left, 0, 0, 2, 2);
        assert_eq!(top_left.stride.get(), 7 * 4);
        assert_eq!(bitmap_rows(&top_left), numbered_pixels(2, 2, 50));
        assert_geometry(&bottom_right, 5, 4, 2, 2);
        assert_eq!(bottom_right.stride.get(), 7 * 4);
        assert_eq!(bitmap_rows(&bottom_right), numbered_pixels(2, 2, 150));
    }

    #[tokio::test(start_paused = true)]
    async fn surface_size_change_yields_resize_then_whole_surface() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        handles
            .surface_mirror
            .lock()
            .await
            .apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 16,
                height: 12,
            });
        blit(&handles, 0, 0, 16, 12, 200).await;

        assert_eq!(
            next_resize(&mut updates).await,
            DesktopSize {
                width: 16,
                height: 12
            }
        );
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 16, 12);
        assert_eq!(bitmap.stride.get(), 16 * 4);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(16, 12, 200));
    }

    #[tokio::test(start_paused = true)]
    async fn mark_all_damaged_yields_the_whole_surface() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        handles.surface_mirror.lock().await.mark_all_damaged();
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
        assert_eq!(bitmap_rows(&bitmap), surface_pixels(&handles).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_replaced_surface_of_the_same_size_is_sent_whole() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        handles
            .surface_mirror
            .lock()
            .await
            .apply_event(&ChannelEvent::SurfaceDestroyed {
                display_channel_id: 0,
                surface_id: 0,
            });
        tokio::time::timeout(DAMAGE_POLL_INTERVAL * 3, updates.next_update())
            .await
            .expect_err("no update without a surface");
        {
            let mut mirror = handles.surface_mirror.lock().await;
            mirror.apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 8,
                height: 6,
            });
            // Clear the new surface's damage, so only the reseed can
            // explain a whole-surface bitmap.
            mirror.primary_surface_mut().expect("surface").take_damage();
        }

        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
        assert_eq!(bitmap_rows(&bitmap), surface_pixels(&handles).await);
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_while_waiting_on_the_tick_keeps_the_damage() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        blit(&handles, 1, 1, 2, 2, 30).await;
        // Shorter than the poll interval, so the call is dropped while
        // it waits on the tick.
        tokio::time::timeout(Duration::from_millis(1), updates.next_update())
            .await
            .expect_err("dropped before the tick");

        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 1, 1, 2, 2);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(2, 2, 30));
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_while_waiting_on_the_lock_keeps_the_damage() {
        let (slot, handles) = session_with_surface(8, 6);
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        blit(&handles, 4, 2, 3, 3, 60).await;
        {
            // Hold the mirror past the tick, so the call is dropped while
            // it waits on the lock.
            let _held = handles.surface_mirror.lock().await;
            tokio::time::timeout(DAMAGE_POLL_INTERVAL * 3, updates.next_update())
                .await
                .expect_err("dropped while the mirror is locked");
        }

        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 4, 2, 3, 3);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(3, 3, 60));
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_slot_yields_nothing_and_reports_the_fallback() {
        let (_other_slot, handles) = session_with_surface(8, 6);
        let slot = SessionSlot::new();
        let (mut display, mut updates) = connect(&slot).await;
        assert_eq!(display.size().await, FALLBACK);

        tokio::time::timeout(DAMAGE_POLL_INTERVAL * 5, updates.next_update())
            .await
            .expect_err("no update without a session");

        // Once a session is chosen, the stream resizes from the fallback
        // and sends the whole surface.
        slot.set(handles);
        assert_eq!(
            next_resize(&mut updates).await,
            DesktopSize {
                width: 8,
                height: 6
            }
        );
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
    }
}
