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
//!
//! The guest's cursor shape rides the same stream, since IronRDP has
//! no other path to the client. It is read the same way: the stream
//! looks at the session's cursor watch on each poll and sends the
//! state as it is then.
//!
//! The handler also asks the guest to fit the client: for the desktop
//! size the client connected with, and for the monitor layout the
//! client sends whenever its window changes size. The guest's answer,
//! if it has an agent to give one, is a new surface size, which the
//! stream sends to the client as a resize like any other, except in
//! the first moments of a connection, when a resize is held back (see
//! [`FIRST_RESIZE_GRACE`]).

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use shakenfist_spice_renderer::channels::align_monitor_dimension;
use shakenfist_spice_renderer::{DamageRect, DisplaySize, DisplaySurface, SurfaceMirror};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tracing::debug;

use crate::cursor::{rdp_pointer, CursorState};
use crate::{SessionHandles, SessionSlot};

/// How often the update stream looks at the mirror for new damage.
///
/// The mirror offers no wake-up when it applies a draw, and a
/// subscription to the session's event bus cannot be ordered after
/// the mirror has applied the event, so the stream polls. 16 ms is
/// one frame at 60 Hz: short enough that a poll adds no visible
/// latency, long enough that an idle session costs next to nothing.
const DAMAGE_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// How long after a connection is accepted the update stream holds
/// back a resize of the client.
///
/// A resize deactivates and reactivates the connection, and IronRDP
/// handles the client's dynamic virtual channels badly across that. It
/// starts the static channel that carries them only on a connection's
/// first activation, and while a connection is reactivating it drops
/// every PDU from the client other than its capability confirm. A
/// resize sent as soon as the connection is up lands while the client
/// is still answering the dynamic-channel handshake, its answers are
/// lost, and the dynamic channels never open for the rest of the
/// connection: the client's monitor layouts never arrive, nor does the
/// mouse input a client like FreeRDP sends over its own channel (the
/// middle button and its wheel). The handshake must therefore finish
/// before any reactivation. IronRDP offers no signal for that, so the
/// stream waits this long from the connection's accept instead: long
/// enough for the rest of the connection sequence and the handshake's
/// few round trips on any network an interactive session is usable
/// over, and short enough that the client waiting for its first frame
/// is no great cost.
///
/// While it waits, the stream sends no bitmaps either, since the
/// surface does not fit the desktop the client has. Pointer updates
/// are not held: they do not reactivate the connection.
///
/// The guest is asked for the client's size rounded down to a
/// multiple of 8, so a guest that adopts it within the hold still
/// needs a resize when the client's size is not such a multiple: the
/// client then waits out the whole hold before its first frame.
const FIRST_RESIZE_GRACE: Duration = Duration::from_secs(2);

/// Bytes per pixel of the surface mirror's RGBA pixels, which are
/// sent to the client as they are, in [`PixelFormat::RgbA32`].
const BYTES_PER_PIXEL: usize = 4;

/// The desktop size the connected client has been told, shared by the
/// display handler, its update streams and the server's connection
/// handler.
///
/// IronRDP asks the display handler for the desktop size twice per
/// connection: once before the handshake, and once after the
/// capability exchange, which is the size the server builds its
/// encoder with. The client is told neither. It is told the desktop
/// size it asked for in its Client Core Data, when that is within
/// RDP's limits, and otherwise the size reported at the first ask; the
/// server then passes the size it was told to `request_initial_size`,
/// which records it here. In TLS mode the size reported at the first
/// ask is the fallback, because the credential check that fills the
/// session slot has not run yet. Either way the client and the encoder
/// can start at different sizes, so the update stream starts from the
/// size the client was told, not the size last reported, and when that
/// is not the surface's size it resizes the client to the surface's;
/// the deactivation and reactivation that follows announces that size
/// to the client and rebuilds the encoder with it. That resize waits
/// until [`FIRST_RESIZE_GRACE`] has passed since the connection was
/// accepted, which is recorded here too, and the stream sends nothing
/// but pointer updates until then. If the guest adopts the size the
/// client was told within that time, no resize is needed at all.
///
/// The first ask records its size too, which stands until the
/// capability exchange replaces it. The server calls
/// `request_initial_size` only for a client that can be resized, so
/// for any other client the first ask's size is all there is.
///
/// The connection handler starts the record afresh when a connection
/// is accepted, so the first ask of every connection records afresh,
/// the first size negotiated on every connection is recognised as the
/// one the client asked for, and the connection's grace period runs
/// from then. Clones share the same record.
#[derive(Clone, Default)]
pub(crate) struct ClientSize(Arc<Mutex<ClientSizeRecord>>);

#[derive(Default)]
struct ClientSizeRecord {
    /// The size the client was last told, if any.
    size: Option<DesktopSize>,
    /// Whether a capability exchange of this connection has told the
    /// client its size.
    negotiated: bool,
    /// When the connection was accepted; `None` before any has been.
    accepted_at: Option<Instant>,
}

impl ClientSize {
    /// Forget the client's size, and note the time, for a connection
    /// accepted now.
    pub(crate) fn start_connection(&self) {
        *self.lock() = ClientSizeRecord {
            accepted_at: Some(Instant::now()),
            ..ClientSizeRecord::default()
        };
    }

    /// When the connection's grace period for resizing the client
    /// ends, or `None` if no connection has been accepted.
    pub(crate) fn resize_held_until(&self) -> Option<Instant> {
        self.lock()
            .accepted_at
            .map(|accepted| accepted + FIRST_RESIZE_GRACE)
    }

    /// The size the client was last told, if any.
    pub(crate) fn get(&self) -> Option<DesktopSize> {
        self.lock().size
    }

    /// Record that the client has been told `size`.
    pub(crate) fn set(&self, size: DesktopSize) {
        self.lock().size = Some(size);
    }

    /// Record that a capability exchange has told the client `size`,
    /// and return whether it is this connection's first: the size the
    /// client asked for, rather than a reactivation's echo of a resize.
    pub(crate) fn negotiated(&self, size: DesktopSize) -> bool {
        let mut record = self.lock();
        record.size = Some(size);
        let first = !record.negotiated;
        record.negotiated = true;
        first
    }

    /// Lock the record. A panic while the lock was held cannot leave
    /// it half-written, so a poisoned lock is recovered rather than
    /// propagated.
    fn lock(&self) -> MutexGuard<'_, ClientSizeRecord> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The RDP display handler for the session in a [`SessionSlot`].
///
/// Reports the session's primary surface size as the desktop size and
/// hands the server an update stream that reads the surface mirror's
/// damage. Before a session has been chosen, or before its first
/// surface exists, the desktop is `fallback` in size; the update
/// stream corrects that with a resize once a surface appears. Sizes
/// the client asks for are passed to the session as resize requests.
pub struct MirrorDisplay {
    slot: SessionSlot,
    fallback: DesktopSize,
    /// The size the connected client was told, which a new update
    /// stream starts from.
    client_size: ClientSize,
}

impl MirrorDisplay {
    /// A display handler for whatever session `slot` holds, reporting
    /// `fallback` as the desktop size while there is no surface to
    /// measure.
    pub fn new(slot: SessionSlot, fallback: DesktopSize) -> Self {
        Self {
            slot,
            fallback,
            client_size: ClientSize::default(),
        }
    }

    /// A shared handle on the record of the client's desktop size, for
    /// the connection handler to start afresh when a new connection is
    /// accepted.
    pub(crate) fn client_size(&self) -> ClientSize {
        self.client_size.clone()
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

    /// Ask the session's guest for a `width` x `height` display,
    /// aligned as the GUI aligns the sizes it asks for.
    ///
    /// Never waits, because the server calls `request_layout` on a
    /// blocking thread with its display lock held. A request the
    /// session cannot take now is dropped; the client's next layout
    /// supersedes it anyway.
    fn request_guest_size(&self, width: u32, height: u32) {
        let Some(session) = self.slot.current() else {
            shakenfist_spice_protocol::warn_once!(
                "rdp-display:resize-no-session",
                "rdp display: the client asked for {}x{} with no session chosen; dropping the request",
                width,
                height
            );
            return;
        };
        let (width, height) = (
            align_monitor_dimension(width),
            align_monitor_dimension(height),
        );
        debug!(
            "rdp display: asking {} for a {}x{} display",
            session.label, width, height
        );
        match session
            .resize_tx
            .try_send(DisplaySize::Requested(width, height))
        {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => shakenfist_spice_protocol::warn_once!(
                "rdp-display:resize-queue-full",
                "rdp display: the resize queue for {} is full; dropping a request for {}x{}",
                session.label,
                width,
                height
            ),
            Err(TrySendError::Closed(_)) => shakenfist_spice_protocol::warn_once!(
                "rdp-display:resize-queue-closed",
                "rdp display: the resize queue for {} is closed; dropping a request for {}x{}",
                session.label,
                width,
                height
            ),
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MirrorDisplay {
    async fn size(&mut self) -> DesktopSize {
        let size = self.current_size().await;
        // The first ask of a connection is the size the client has
        // until the capability exchange says otherwise; later asks of
        // the same connection do not change that.
        if self.client_size.get().is_none() {
            self.client_size.set(size);
        }
        size
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        // The size the client has just been told, which replaces the
        // one recorded at the first ask. The update stream starts from
        // it, so when the surface is another size the stream resizes
        // the client to the surface's, once the connection's grace
        // period has passed.
        if self.client_size.negotiated(client_size) {
            // The first of a connection is the size the client asked
            // for. Later ones follow a reactivation and echo the size
            // the stream resized the client to, which is the guest's
            // own; asking the guest for that would undo the request.
            self.request_guest_size(u32::from(client_size.width), u32::from(client_size.height));
        }
        client_size
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let announced = match self.client_size.get() {
            Some(size) => size,
            None => self.size().await,
        };
        Ok(Box::new(MirrorUpdates::new(
            self.slot.clone(),
            self.client_size.clone(),
            announced,
        )))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some((width, height)) = layout_size(&layout) else {
            debug!("rdp display: the client sent a monitor layout with no monitors");
            return;
        };
        // A client that fits its window to its desktop reports the
        // window's new size after every resize the stream sends, so a
        // layout of the size the client already has is that echo, not
        // a request. Passing it on would ask the guest for its own old
        // size, superseding a request it has not yet answered.
        let has = self
            .client_size
            .get()
            .map(|size| (u32::from(size.width), u32::from(size.height)));
        if has == Some((width, height)) {
            debug!(
                "rdp display: the client's layout is the {}x{} it already has; ignoring it",
                width, height
            );
            return;
        }
        self.request_guest_size(width, height);
    }
}

/// The size of `layout`'s primary monitor, or of its first monitor when
/// none is marked primary, which decoding does not insist on; `None`
/// for a layout with no monitors. Only one monitor is served, so the
/// others are ignored.
fn layout_size(layout: &DisplayControlMonitorLayout) -> Option<(u32, u32)> {
    let monitors = layout.monitors();
    monitors
        .iter()
        .find(|monitor| monitor.is_primary())
        .or_else(|| monitors.first())
        .map(MonitorLayoutEntry::dimensions)
}

/// One connection's stream of display updates, read from the mirror.
///
/// The server builds a new stream for every connection, and again
/// after every resize (a resize deactivates and reactivates the
/// connection), so a stream starts unseeded and sends the whole
/// surface first. A reactivated client also starts from its default
/// pointer, so a stream sends the cursor state before anything else.
///
/// Within [`FIRST_RESIZE_GRACE`] of the connection's accept, a surface
/// that is not the size the client has is neither resized to nor sent:
/// the stream sends only pointer updates and keeps polling. When the
/// grace period ends it resizes the client and sends the whole surface,
/// or, if the surface has become the client's size meanwhile, sends the
/// whole surface with no resize. A stream built after the grace period,
/// as every stream after a reactivation is, resizes at once.
struct MirrorUpdates {
    slot: SessionSlot,
    /// Updated with every resize sent, since the reactivation that
    /// follows tells the client that size.
    client_size: ClientSize,
    /// Updates taken from the mirror and not yet handed to the server.
    pending: VecDeque<DisplayUpdate>,
    /// The desktop size the client currently has: the size it was
    /// told when the stream was built, or the last resize sent.
    sent_size: DesktopSize,
    /// Until when a surface of another size than `sent_size` is held
    /// back rather than resized to; `None` once that has passed, or if
    /// no connection was recorded as accepted.
    resize_held_until: Option<Instant>,
    /// Whether the client has been sent the whole surface at
    /// `sent_size`. IronRDP seeds its framebuffer only from a bitmap
    /// covering the whole desktop, and without one it re-encodes every
    /// later bitmap in full instead of diffing it, so the first bitmap
    /// of a stream, and the first after a resize, must be the whole
    /// surface.
    seeded: bool,
    /// The cursor watch of the session last polled, taken afresh when
    /// the slot's session changes. `None` until the first poll with a
    /// session.
    cursor: Option<CursorWatch>,
    interval: Interval,
}

/// A session's cursor watch, and which session it belongs to.
struct CursorWatch {
    /// Weak, so the stream does not keep a session alive; holding it
    /// keeps the allocation, so a later session cannot share its
    /// address.
    session: Weak<SessionHandles>,
    state: watch::Receiver<CursorState>,
}

impl MirrorUpdates {
    fn new(slot: SessionSlot, client_size: ClientSize, announced: DesktopSize) -> Self {
        let mut interval = tokio::time::interval(DAMAGE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let resize_held_until = client_size.resize_held_until();
        Self {
            slot,
            client_size,
            pending: VecDeque::new(),
            sent_size: announced,
            resize_held_until,
            seeded: false,
            cursor: None,
            interval,
        }
    }

    /// Look at `session`'s cursor once, and queue a pointer update if
    /// the client has not been sent its current state: when the watch
    /// has changed, and whatever it says when the watch has just been
    /// taken, for a new stream or a new session.
    ///
    /// Synchronous for the same reason as [`Self::poll_mirror`]: the
    /// state read from the watch is in `pending` before this returns.
    fn poll_cursor(&mut self, session: &Arc<SessionHandles>) {
        let (cursor, taken) = match &mut self.cursor {
            Some(cursor) if Weak::as_ptr(&cursor.session) == Arc::as_ptr(session) => {
                (cursor, false)
            }
            other => {
                let cursor = other.insert(CursorWatch {
                    session: Arc::downgrade(session),
                    state: session.cursor.clone(),
                });
                (cursor, true)
            }
        };
        // A closed watch (the relay has ended with the session) still
        // holds the last state, and will not change again.
        let changed = cursor.state.has_changed().unwrap_or(false);
        if !(changed || taken) {
            return;
        }
        let state = cursor.state.borrow_and_update().clone();
        if let Some(update) = pointer_update(&state) {
            self.pending.push_back(update);
        }
    }

    /// Whether a resize of the client must still be held back, because
    /// the connection's grace period has not passed.
    fn resize_held(&mut self) -> bool {
        match self.resize_held_until {
            Some(until) if Instant::now() < until => true,
            _ => {
                self.resize_held_until = None;
                false
            }
        }
    }

    /// Look at the mirror once and queue whatever the client needs:
    /// a resize and the whole surface if the surface changed size
    /// (nothing at all while that resize is held), the whole surface if
    /// the client has not had it yet, and otherwise one bitmap per
    /// damaged rectangle.
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
            if self.resize_held() {
                // Bitmaps of this surface would not fit the client's
                // desktop, so nothing is sent until the resize is. The
                // client's view is stale by then, so whatever is sent
                // next is the whole surface.
                self.seeded = false;
                return;
            }
            debug!(
                "rdp display: surface is {}x{}, client has {}x{}; resizing",
                size.width, size.height, self.sent_size.width, self.sent_size.height
            );
            self.pending.push_back(DisplayUpdate::Resize(size));
            self.client_size.set(size);
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
        // taken stays in `self.pending` for the next call. The cursor
        // watch is read the same way, in `poll_cursor`, before the
        // lock, so a pointer update goes out ahead of that poll's
        // bitmaps and is already queued if the call is dropped at the
        // lock. The mirror lock is held only for that synchronous step,
        // so the SPICE side is never kept waiting on an encode or a
        // network write.
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
            self.poll_cursor(&session);
            let mut mirror = session.surface_mirror.lock().await;
            self.poll_mirror(&mut mirror);
        }
    }
}

/// The pointer update that shows the client `state`: hidden, the
/// client's default pointer before the guest has sent a shape, and
/// otherwise the shape. `None` for a shape with no pixels, which
/// leaves the client's pointer as it was.
fn pointer_update(state: &CursorState) -> Option<DisplayUpdate> {
    if !state.visible {
        return Some(DisplayUpdate::HidePointer);
    }
    match &state.shape {
        None => Some(DisplayUpdate::DefaultPointer),
        Some(shape) => rdp_pointer(shape).map(DisplayUpdate::RGBAPointer),
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

    use ironrdp_server::RGBAPointer;
    use shakenfist_spice_renderer::{ChannelEvent, CursorImage};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::*;

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
    /// primary surface of numbered pixels, and whose cursor watch
    /// stays at the default state: no shape, visible.
    fn session_with_surface(width: u32, height: u32) -> (SessionSlot, Arc<SessionHandles>) {
        let (slot, handles, _cursor) = session_with_cursor(width, height, CursorState::default());
        (slot, handles)
    }

    /// As [`session_with_surface`], with the cursor watch starting at
    /// `cursor`, and its sending end.
    fn session_with_cursor(
        width: u32,
        height: u32,
        cursor: CursorState,
    ) -> (SessionSlot, Arc<SessionHandles>, watch::Sender<CursorState>) {
        let (slot, handles, cursor_tx, _resize) = session(width, height, cursor);
        (slot, handles, cursor_tx)
    }

    /// As [`session_with_surface`], and the receiving end of the
    /// session's resize queue.
    fn session_with_resize(
        width: u32,
        height: u32,
    ) -> (
        SessionSlot,
        Arc<SessionHandles>,
        mpsc::Receiver<DisplaySize>,
    ) {
        let (slot, handles, _cursor, resize) = session(width, height, CursorState::default());
        (slot, handles, resize)
    }

    /// How many requests a test session's resize queue holds.
    const RESIZE_QUEUE: usize = 4;

    /// A slot holding a session whose mirror has one `width` x `height`
    /// primary surface of numbered pixels and whose cursor watch starts
    /// at `cursor`, with the sending end of that watch and the
    /// receiving end of the session's resize queue.
    fn session(
        width: u32,
        height: u32,
        cursor: CursorState,
    ) -> (
        SessionSlot,
        Arc<SessionHandles>,
        watch::Sender<CursorState>,
        mpsc::Receiver<DisplaySize>,
    ) {
        let mirror = SurfaceMirror::with_test_surface(
            0,
            0,
            width,
            height,
            &numbered_pixels(width, height, 0),
        );
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = watch::channel(0);
        let (resize_tx, resize_rx) = mpsc::channel(RESIZE_QUEUE);
        let (cursor_tx, cursor) = watch::channel(cursor);
        let handles = Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            label: String::from("test"),
        });
        let slot = SessionSlot::new();
        slot.set(Arc::clone(&handles));
        (slot, handles, cursor_tx, resize_rx)
    }

    /// A display over `slot`'s session and its update stream, built as
    /// the server builds them: size first, then updates.
    async fn open(slot: &SessionSlot) -> (MirrorDisplay, Box<dyn RdpServerDisplayUpdates>) {
        let mut display = MirrorDisplay::new(slot.clone(), FALLBACK);
        display.size().await;
        let updates = display.updates().await.expect("updates");
        (display, updates)
    }

    /// As [`open`], for a session whose cursor is in the default state,
    /// with the default pointer the stream sends first already taken.
    async fn connect(slot: &SessionSlot) -> (MirrorDisplay, Box<dyn RdpServerDisplayUpdates>) {
        let (display, mut updates) = open(slot).await;
        next_default_pointer(&mut updates).await;
        (display, updates)
    }

    /// The next update. The stream waits for ever while it has nothing
    /// to send, so a missing update fails the test after a few hundred
    /// polls of paused time rather than hanging it.
    async fn next(updates: &mut Box<dyn RdpServerDisplayUpdates>) -> DisplayUpdate {
        tokio::time::timeout(DAMAGE_POLL_INTERVAL * 300, updates.next_update())
            .await
            .expect("an update within 300 polls")
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

    async fn next_pointer(updates: &mut Box<dyn RdpServerDisplayUpdates>) -> RGBAPointer {
        match next(updates).await {
            DisplayUpdate::RGBAPointer(pointer) => pointer,
            other => panic!("expected a pointer, got {other:?}"),
        }
    }

    async fn next_default_pointer(updates: &mut Box<dyn RdpServerDisplayUpdates>) {
        match next(updates).await {
            DisplayUpdate::DefaultPointer => {}
            other => panic!("expected the default pointer, got {other:?}"),
        }
    }

    async fn next_hide_pointer(updates: &mut Box<dyn RdpServerDisplayUpdates>) {
        match next(updates).await {
            DisplayUpdate::HidePointer => {}
            other => panic!("expected a hidden pointer, got {other:?}"),
        }
    }

    /// A monitor layout as a client sends it, each monitor given as its
    /// primary flag, width and height, decoded as the server decodes
    /// it.
    fn wire_layout(monitors: &[(bool, u32, u32)]) -> DisplayControlMonitorLayout {
        // MonitorLayoutSize, then NumMonitors, then ten 32-bit fields
        // per monitor: flags, left, top, width, height, physical width
        // and height, orientation, and the two scale factors.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&(monitors.len() as u32).to_le_bytes());
        for &(primary, width, height) in monitors {
            for field in [u32::from(primary), 0, 0, width, height, 0, 0, 0, 0, 0] {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
        }
        ironrdp_pdu::decode(&bytes).expect("a valid monitor layout")
    }

    /// Every resize request queued so far.
    fn requests(resize: &mut mpsc::Receiver<DisplaySize>) -> Vec<DisplaySize> {
        std::iter::from_fn(|| resize.try_recv().ok()).collect()
    }

    /// A display over `slot`'s session for a connection accepted now,
    /// built as the server builds it for a client told `told` in the
    /// capability exchange: started afresh, size, the negotiated size,
    /// then size again for the encoder.
    async fn accept_told(slot: &SessionSlot, told: DesktopSize) -> MirrorDisplay {
        let mut display = MirrorDisplay::new(slot.clone(), FALLBACK);
        display.client_size().start_connection();
        display.size().await;
        display.request_initial_size(told).await;
        display.size().await;
        display
    }

    /// No update before `deadline`.
    async fn assert_quiet_until(updates: &mut Box<dyn RdpServerDisplayUpdates>, deadline: Instant) {
        if let Ok(update) = tokio::time::timeout_at(deadline, updates.next_update()).await {
            panic!("expected no update before the deadline, got {update:?}");
        }
    }

    /// No update within a few polls.
    async fn assert_quiet(updates: &mut Box<dyn RdpServerDisplayUpdates>) {
        if let Ok(update) =
            tokio::time::timeout(DAMAGE_POLL_INTERVAL * 5, updates.next_update()).await
        {
            panic!("expected no update, got {update:?}");
        }
    }

    /// A `width` x `height` cursor of one opaque colour, with its hot
    /// spot at (1, 1).
    fn cursor_image(width: u16, height: u16, [r, g, b]: [u8; 3]) -> Arc<CursorImage> {
        Arc::new(CursorImage {
            width,
            height,
            hot_spot_x: 1,
            hot_spot_y: 1,
            pixels: [r, g, b, 0xFF].repeat(usize::from(width) * usize::from(height)),
        })
    }

    fn showing(shape: &Arc<CursorImage>) -> CursorState {
        CursorState {
            shape: Some(Arc::clone(shape)),
            visible: true,
        }
    }

    /// Assert `pointer` is `width` x `height`, hot spot (1, 1), and one
    /// colour, given as RGB and expected on the wire as BGRA.
    fn assert_pointer(pointer: &RGBAPointer, width: u16, height: u16, [r, g, b]: [u8; 3]) {
        assert_eq!(
            (pointer.width, pointer.height, pointer.hot_x, pointer.hot_y),
            (width, height, 1, 1)
        );
        assert_eq!(
            pointer.data,
            [b, g, r, 0xFF].repeat(usize::from(width) * usize::from(height))
        );
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
    async fn a_session_chosen_after_the_first_size_starts_with_a_resize() {
        // The TLS-mode sequence: the client is told the size before
        // authentication, while the slot is empty; the credential check
        // then fills the slot; the encoder is sized after that.
        let (_other_slot, handles) = session_with_surface(8, 6);
        let slot = SessionSlot::new();
        let mut display = MirrorDisplay::new(slot.clone(), FALLBACK);
        assert_eq!(display.size().await, FALLBACK);
        slot.set(handles);
        let surface = DesktopSize {
            width: 8,
            height: 6,
        };
        assert_eq!(display.size().await, surface);
        assert_eq!(display.client_size().get(), Some(FALLBACK));

        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;
        assert_eq!(next_resize(&mut updates).await, surface);
        assert_eq!(display.client_size().get(), Some(surface));
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_client_size_is_recorded_afresh() {
        let (slot, handles) = session_with_surface(8, 6);
        let mut display = MirrorDisplay::new(slot.clone(), FALLBACK);
        display.size().await;
        let first = DesktopSize {
            width: 8,
            height: 6,
        };
        assert_eq!(display.client_size().get(), Some(first));

        // A second connection, after the guest changed its resolution.
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
        let second = DesktopSize {
            width: 16,
            height: 12,
        };
        // Until a new connection starts, the record keeps the first
        // connection's size.
        assert_eq!(display.size().await, second);
        assert_eq!(display.client_size().get(), Some(first));

        display.client_size().start_connection();
        assert_eq!(display.client_size().get(), None);
        assert_eq!(display.size().await, second);
        assert_eq!(display.client_size().get(), Some(second));

        // The new connection's stream starts from the recorded size, so
        // it seeds without a resize.
        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 16, 12);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_slot_yields_nothing_and_reports_the_fallback() {
        let (_other_slot, handles) = session_with_surface(8, 6);
        let slot = SessionSlot::new();
        let (mut display, mut updates) = open(&slot).await;
        assert_eq!(display.size().await, FALLBACK);

        tokio::time::timeout(DAMAGE_POLL_INTERVAL * 5, updates.next_update())
            .await
            .expect_err("no update without a session");

        // Once a session is chosen, the stream sends its cursor, resizes
        // from the fallback and sends the whole surface.
        slot.set(handles);
        next_default_pointer(&mut updates).await;
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

    #[tokio::test(start_paused = true)]
    async fn the_first_negotiated_size_is_recorded_and_asked_for_aligned() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.size().await;

        let asked = DesktopSize {
            width: 1283,
            height: 717,
        };
        assert_eq!(display.request_initial_size(asked).await, asked);
        assert_eq!(display.client_size().get(), Some(asked));
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(1280, 712)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reactivation_records_its_size_without_asking_the_guest() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.size().await;
        display
            .request_initial_size(DesktopSize {
                width: 800,
                height: 600,
            })
            .await;
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(800, 600)]);

        // The client's echo of the surface's size it was resized to.
        let surface = DesktopSize {
            width: 64,
            height: 48,
        };
        assert_eq!(display.request_initial_size(surface).await, surface);
        assert_eq!(display.client_size().get(), Some(surface));
        assert_eq!(requests(&mut resize), []);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_connection_asks_the_guest_again() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.size().await;
        display
            .request_initial_size(DesktopSize {
                width: 800,
                height: 600,
            })
            .await;
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(800, 600)]);

        display.client_size().start_connection();
        display.size().await;
        let asked = DesktopSize {
            width: 1024,
            height: 768,
        };
        display.request_initial_size(asked).await;
        assert_eq!(display.client_size().get(), Some(asked));
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(1024, 768)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_negotiated_size_other_than_the_surface_starts_with_a_resize() {
        let (slot, _handles, _resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        let surface = DesktopSize {
            width: 64,
            height: 48,
        };
        assert_eq!(display.size().await, surface);
        display
            .request_initial_size(DesktopSize {
                width: 1280,
                height: 720,
            })
            .await;
        // The server sizes its encoder from a second ask.
        assert_eq!(display.size().await, surface);

        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;
        assert_eq!(next_resize(&mut updates).await, surface);
        assert_eq!(display.client_size().get(), Some(surface));
        assert_geometry(&next_bitmap(&mut updates).await, 0, 0, 64, 48);
    }

    #[test]
    fn a_layout_asks_for_its_primary_monitor() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.request_layout(wire_layout(&[(false, 1024, 768), (true, 1366, 802)]));
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(1360, 800)]);
    }

    #[test]
    fn a_layout_without_a_primary_asks_for_its_first_monitor() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.request_layout(wire_layout(&[(false, 1922, 1080), (false, 1024, 768)]));
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(1920, 1080)]);
    }

    #[test]
    fn a_layout_of_the_size_the_client_has_asks_for_nothing() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.client_size().set(DesktopSize {
            width: 1024,
            height: 768,
        });
        // The client fitting its window to a resize the stream sent.
        display.request_layout(wire_layout(&[(true, 1024, 768)]));
        assert_eq!(requests(&mut resize), []);
        // A window the user resized is a request.
        display.request_layout(wire_layout(&[(true, 1280, 768)]));
        assert_eq!(requests(&mut resize), [DisplaySize::Requested(1280, 768)]);
    }

    #[test]
    fn a_layout_without_monitors_asks_for_nothing() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        display.request_layout(wire_layout(&[]));
        assert_eq!(requests(&mut resize), []);
    }

    #[test]
    fn a_layout_with_no_session_is_dropped() {
        let mut display = MirrorDisplay::new(SessionSlot::new(), FALLBACK);
        display.request_layout(wire_layout(&[(true, 1024, 768)]));
    }

    #[test]
    fn a_layout_the_full_queue_cannot_take_is_dropped() {
        let (slot, _handles, mut resize) = session_with_resize(64, 48);
        let mut display = MirrorDisplay::new(slot, FALLBACK);
        for width in (0..=RESIZE_QUEUE as u32).map(|i| 1024 + 8 * i) {
            display.request_layout(wire_layout(&[(true, width, 768)]));
        }
        // The last one found the queue full; the ones before it stand.
        let expected: Vec<_> = (0..RESIZE_QUEUE as u32)
            .map(|i| DisplaySize::Requested(1024 + 8 * i, 768))
            .collect();
        assert_eq!(requests(&mut resize), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn first_poll_sends_the_cursor_before_the_whole_surface() {
        let shape = cursor_image(3, 2, [0x10, 0x80, 0xF0]);
        let (slot, _handles, _cursor) = session_with_cursor(8, 6, showing(&shape));
        let (_display, mut updates) = open(&slot).await;

        assert_pointer(&next_pointer(&mut updates).await, 3, 2, [0x10, 0x80, 0xF0]);
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 8, 6);
        assert_quiet(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_hidden_cursor_is_sent_hidden() {
        let hidden = CursorState {
            shape: Some(cursor_image(3, 2, [1, 2, 3])),
            visible: false,
        };
        let (slot, _handles, _cursor) = session_with_cursor(8, 6, hidden);
        let (_display, mut updates) = open(&slot).await;

        next_hide_pointer(&mut updates).await;
        next_bitmap(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_cursor_change_mid_stream_sends_one_pointer_update() {
        let (slot, handles, cursor) = session_with_cursor(8, 6, CursorState::default());
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;
        assert_quiet(&mut updates).await;

        let shape = cursor_image(4, 4, [0xC0, 0x20, 0x08]);
        cursor.send_replace(showing(&shape));
        blit(&handles, 2, 1, 3, 2, 100).await;
        // The pointer goes ahead of the same poll's damage.
        assert_pointer(&next_pointer(&mut updates).await, 4, 4, [0xC0, 0x20, 0x08]);
        assert_geometry(&next_bitmap(&mut updates).await, 2, 1, 3, 2);
        assert_quiet(&mut updates).await;

        // Hiding sends the hidden pointer; showing again resends the
        // shape.
        cursor.send_modify(|state| state.visible = false);
        next_hide_pointer(&mut updates).await;
        assert_quiet(&mut updates).await;
        cursor.send_modify(|state| state.visible = true);
        assert_pointer(&next_pointer(&mut updates).await, 4, 4, [0xC0, 0x20, 0x08]);
        assert_quiet(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_stream_resends_the_cursor() {
        // The watch never changes, so only the stream itself can ask
        // for the cursor to be sent.
        let shape = cursor_image(2, 2, [9, 8, 7]);
        let (slot, _handles, _cursor) = session_with_cursor(8, 6, showing(&shape));
        let (mut display, mut updates) = open(&slot).await;
        next_pointer(&mut updates).await;
        next_bitmap(&mut updates).await;

        // As after a reactivation.
        drop(updates);
        let mut updates = display.updates().await.expect("updates");
        assert_pointer(&next_pointer(&mut updates).await, 2, 2, [9, 8, 7]);
        next_bitmap(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_session_sends_its_cursor() {
        let (slot, handles, cursor) = session_with_cursor(8, 6, CursorState::default());
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        // A second session on the same mirror, whose cursor watch has
        // not changed since it was made.
        let shape = cursor_image(5, 3, [0x33, 0x66, 0x99]);
        let (_second_tx, second_cursor) = watch::channel(showing(&shape));
        slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::clone(&handles.surface_mirror),
            input_tx: handles.input_tx.clone(),
            mouse_mode: handles.mouse_mode.clone(),
            resize_tx: handles.resize_tx.clone(),
            cursor: second_cursor,
            label: String::from("second"),
        }));
        assert_pointer(&next_pointer(&mut updates).await, 5, 3, [0x33, 0x66, 0x99]);
        assert_quiet(&mut updates).await;

        // The first session's watch is no longer read.
        cursor.send_modify(|state| state.visible = false);
        assert_quiet(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_while_waiting_on_the_lock_keeps_the_cursor() {
        let (slot, handles, cursor) = session_with_cursor(8, 6, CursorState::default());
        let (_display, mut updates) = connect(&slot).await;
        next_bitmap(&mut updates).await;

        let shape = cursor_image(2, 3, [0x40, 0x50, 0x60]);
        cursor.send_replace(showing(&shape));
        {
            let _held = handles.surface_mirror.lock().await;
            tokio::time::timeout(DAMAGE_POLL_INTERVAL * 3, updates.next_update())
                .await
                .expect_err("dropped while the mirror is locked");
        }

        assert_pointer(&next_pointer(&mut updates).await, 2, 3, [0x40, 0x50, 0x60]);
        assert_quiet(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_resize_waits_for_the_grace_period_with_no_bitmap_before_it() {
        let (slot, _handles) = session_with_surface(64, 48);
        let accepted = Instant::now();
        let mut display = accept_told(
            &slot,
            DesktopSize {
                width: 1280,
                height: 720,
            },
        )
        .await;
        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;

        // Neither the resize nor a bitmap of the surface, which would
        // not fit the client's desktop, until the grace period ends.
        let held_until = accepted + FIRST_RESIZE_GRACE;
        assert_quiet_until(&mut updates, held_until - DAMAGE_POLL_INTERVAL).await;

        let surface = DesktopSize {
            width: 64,
            height: 48,
        };
        assert_eq!(next_resize(&mut updates).await, surface);
        assert!(Instant::now() >= held_until);
        assert!(Instant::now() <= held_until + DAMAGE_POLL_INTERVAL);
        assert_eq!(display.client_size().get(), Some(surface));
        let bitmap = next_bitmap(&mut updates).await;
        assert_geometry(&bitmap, 0, 0, 64, 48);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(64, 48, 0));
        assert_quiet(&mut updates).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_adopting_the_told_size_in_the_grace_period_needs_no_resize() {
        let (slot, handles) = session_with_surface(64, 48);
        let accepted = Instant::now();
        let told = DesktopSize {
            width: 128,
            height: 96,
        };
        let mut display = accept_told(&slot, told).await;
        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;
        assert_quiet(&mut updates).await;

        // The guest answers the request for the client's size.
        handles
            .surface_mirror
            .lock()
            .await
            .apply_event(&ChannelEvent::SurfaceCreated {
                display_channel_id: 0,
                surface_id: 0,
                width: 128,
                height: 96,
            });
        blit(&handles, 0, 0, 128, 96, 200).await;

        let bitmap = next_bitmap(&mut updates).await;
        assert!(Instant::now() < accepted + FIRST_RESIZE_GRACE);
        assert_geometry(&bitmap, 0, 0, 128, 96);
        assert_eq!(bitmap_rows(&bitmap), numbered_pixels(128, 96, 200));
        // The client already has the surface's size, so nothing follows
        // when the grace period ends.
        assert_quiet_until(
            &mut updates,
            accepted + FIRST_RESIZE_GRACE + DAMAGE_POLL_INTERVAL * 5,
        )
        .await;
        assert_eq!(display.client_size().get(), Some(told));
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_resize_in_the_grace_period_waits_for_it() {
        let (slot, handles) = session_with_surface(64, 48);
        let accepted = Instant::now();
        let surface = DesktopSize {
            width: 64,
            height: 48,
        };
        let mut display = accept_told(&slot, surface).await;
        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;
        // The client was told the surface's size, so it is served at once.
        assert_geometry(&next_bitmap(&mut updates).await, 0, 0, 64, 48);

        // The guest changes size on its own before the grace period ends.
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
        let held_until = accepted + FIRST_RESIZE_GRACE;
        assert_quiet_until(&mut updates, held_until - DAMAGE_POLL_INTERVAL).await;

        let resized = DesktopSize {
            width: 16,
            height: 12,
        };
        assert_eq!(next_resize(&mut updates).await, resized);
        assert!(Instant::now() >= held_until);
        assert_eq!(display.client_size().get(), Some(resized));
        assert_geometry(&next_bitmap(&mut updates).await, 0, 0, 16, 12);
    }

    #[tokio::test(start_paused = true)]
    async fn pointer_updates_are_not_held_in_the_grace_period() {
        let shape = cursor_image(3, 2, [0x10, 0x80, 0xF0]);
        let (slot, _handles, cursor) = session_with_cursor(64, 48, showing(&shape));
        let accepted = Instant::now();
        let mut display = accept_told(
            &slot,
            DesktopSize {
                width: 1280,
                height: 720,
            },
        )
        .await;
        let mut updates = display.updates().await.expect("updates");

        assert_pointer(&next_pointer(&mut updates).await, 3, 2, [0x10, 0x80, 0xF0]);
        assert!(Instant::now() <= accepted + DAMAGE_POLL_INTERVAL);
        cursor.send_modify(|state| state.visible = false);
        next_hide_pointer(&mut updates).await;
        assert!(Instant::now() < accepted + FIRST_RESIZE_GRACE);

        // The resize still waits for the grace period.
        next_resize(&mut updates).await;
        assert!(Instant::now() >= accepted + FIRST_RESIZE_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_built_after_the_grace_period_resizes_at_once() {
        let (slot, _handles) = session_with_surface(64, 48);
        let mut display = accept_told(
            &slot,
            DesktopSize {
                width: 1280,
                height: 720,
            },
        )
        .await;
        // As for a stream built after a reactivation.
        tokio::time::sleep(FIRST_RESIZE_GRACE).await;
        let built = Instant::now();
        let mut updates = display.updates().await.expect("updates");
        next_default_pointer(&mut updates).await;

        assert_eq!(
            next_resize(&mut updates).await,
            DesktopSize {
                width: 64,
                height: 48
            }
        );
        assert!(Instant::now() <= built + DAMAGE_POLL_INTERVAL);
        assert_geometry(&next_bitmap(&mut updates).await, 0, 0, 64, 48);
    }
}
