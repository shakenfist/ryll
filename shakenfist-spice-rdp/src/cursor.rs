//! The guest's cursor, for the RDP client: a relay that keeps the
//! session's latest cursor shape and visibility in a watch channel,
//! and the conversion from a decoded SPICE cursor to an RDP pointer.
//!
//! The renderer announces cursor shapes and visibility as events on
//! the session's event bus, and announces visibility again with every
//! pointer move. The RDP client needs only the latest state, sent when
//! it changes, so the relay folds the events into a
//! [`watch`](tokio::sync::watch) channel that changes only when the
//! shape or the visibility does. A client that falls behind is sent
//! the state as it is by then, never a backlog of shapes.

use std::sync::Arc;

use ironrdp_server::RGBAPointer;
use shakenfist_spice_renderer::{ChannelEvent, CursorImage};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::debug;

/// The largest pointer side, in pixels, sent to an RDP client.
///
/// IronRDP does not advertise the Large Pointer capability set, so a
/// conforming client expects pointers of at most 96x96 and may refuse
/// anything bigger; the wire format's own limit is near 127x127, and a
/// pointer past it fails to encode and drops the client. Larger
/// cursors are scaled down to fit.
const MAX_POINTER_SIDE: u16 = 96;

/// The session's cursor as the RDP client should show it.
#[derive(Clone)]
pub struct CursorState {
    /// The latest cursor shape the guest sent, or `None` before the
    /// first. Kept while the cursor is hidden, so that showing it
    /// again needs no new shape.
    pub shape: Option<Arc<CursorImage>>,
    /// Whether the guest shows the cursor.
    pub visible: bool,
}

impl Default for CursorState {
    /// No shape yet, and visible: the client keeps its own default
    /// pointer until the guest sends a shape.
    fn default() -> Self {
        Self {
            shape: None,
            visible: true,
        }
    }
}

/// Spawn a task that folds the session's cursor events from `events`
/// into a watch channel, and return its receiver and the task.
///
/// Subscribe `events` before the session is spawned: the first cursor
/// shape arrives during session setup, and a broadcast receiver sees
/// only what is sent after it was created. The task ends when the
/// event bus closes; the receiver then keeps the last state.
pub fn spawn_cursor_relay(
    mut events: broadcast::Receiver<ChannelEvent>,
) -> (watch::Receiver<CursorState>, JoinHandle<()>) {
    let (state_tx, state_rx) = watch::channel(CursorState::default());
    let task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => apply_event(&state_tx, event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    // The watch holds whole states rather than
                    // deltas, so the next shape or visibility change
                    // corrects whatever was missed.
                    shakenfist_spice_protocol::warn_once!(
                        "rdp-cursor:lagged",
                        "rdp cursor: missed {} session events; the next cursor change corrects the shape",
                        missed
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("rdp cursor: event bus closed; relay exiting");
                    break;
                }
            }
        }
    });
    (state_rx, task)
}

/// Fold one session event into the cursor state. A shape always
/// changes the state; a visibility report changes it only when the
/// visibility differs, so the reports that come with every pointer
/// move wake no one.
fn apply_event(state: &watch::Sender<CursorState>, event: ChannelEvent) {
    match event {
        ChannelEvent::CursorShape(image) => {
            state.send_modify(|current| current.shape = Some(Arc::new(image)));
        }
        ChannelEvent::CursorPosition { visible, .. } => {
            state.send_if_modified(|current| {
                if current.visible == visible {
                    return false;
                }
                current.visible = visible;
                true
            });
        }
        _ => {}
    }
}

/// The RDP pointer for `image`, or `None` if it has no pixels.
///
/// IronRDP sends an [`RGBAPointer`]'s data unchanged as the XOR mask
/// of a 32 bpp pointer with no AND mask, which a client reads as
/// bottom-up rows of blue, green, red and straight alpha. The decoded
/// cursor is top-down, straight-alpha RGBA, so the rows are flipped
/// and red and blue swapped. A cursor larger than
/// [`MAX_POINTER_SIDE`] in either dimension is scaled down to fit,
/// keeping its aspect ratio, by nearest neighbour; its hot spot is
/// scaled with it. The hot spot is clamped inside the pointer either
/// way. Every pointer goes out with cache index 0.
pub(crate) fn rdp_pointer(image: &CursorImage) -> Option<RGBAPointer> {
    let (src_width, src_height) = (image.width, image.height);
    if src_width == 0 || src_height == 0 {
        return None;
    }
    let src_len = usize::from(src_width) * usize::from(src_height) * 4;
    let src = image.pixels.get(..src_len)?;

    let longest = src_width.max(src_height);
    let (width, height) = if longest > MAX_POINTER_SIDE {
        let scale = |side: u16| {
            let scaled = u32::from(side) * u32::from(MAX_POINTER_SIDE) / u32::from(longest);
            // At most MAX_POINTER_SIDE, so always fits.
            u16::try_from(scaled).unwrap_or(MAX_POINTER_SIDE).max(1)
        };
        (scale(src_width), scale(src_height))
    } else {
        (src_width, src_height)
    };
    // The source coordinate a destination coordinate samples, and the
    // destination of a source hot spot coordinate: the same
    // proportion in each direction, so an unscaled side maps to
    // itself.
    let sample = |dst: u16, dst_side: u16, src_side: u16| {
        usize::try_from(u32::from(dst) * u32::from(src_side) / u32::from(dst_side)).unwrap_or(0)
    };
    let place = |hot: u16, src_side: u16, dst_side: u16| {
        let placed = u32::from(hot) * u32::from(dst_side) / u32::from(src_side);
        u16::try_from(placed).unwrap_or(u16::MAX).min(dst_side - 1)
    };

    let src_stride = usize::from(src_width) * 4;
    let mut data = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
    for row in (0..height).rev() {
        let src_row = sample(row, height, src_height) * src_stride;
        for column in 0..width {
            let at = src_row + sample(column, width, src_width) * 4;
            let [r, g, b, a] = [src[at], src[at + 1], src[at + 2], src[at + 3]];
            data.extend_from_slice(&[b, g, r, a]);
        }
    }

    Some(RGBAPointer {
        cache_index: 0,
        width,
        height,
        hot_x: place(image.hot_spot_x, src_width, width),
        hot_y: place(image.hot_spot_y, src_height, height),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u16, height: u16, hot: (u16, u16), pixels: Vec<u8>) -> CursorImage {
        CursorImage {
            width,
            height,
            hot_spot_x: hot.0,
            hot_spot_y: hot.1,
            pixels,
        }
    }

    /// A `width` x `height` image whose pixel at (x, y) is red x, green
    /// y, blue 0x5A and alpha 0xC3, so a pointer's bytes say which
    /// source pixel each came from and which channel is which.
    fn coordinate_image(width: u16, height: u16, hot: (u16, u16)) -> CursorImage {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[x as u8, y as u8, 0x5A, 0xC3]);
            }
        }
        image(width, height, hot, pixels)
    }

    /// The source (x, y) the pointer pixel at top-down (column, row)
    /// was sampled from, checking the other two channels on the way.
    fn source_of(pointer: &RGBAPointer, column: u16, row: u16) -> (u8, u8) {
        let bottom_up = usize::from(pointer.height - 1 - row);
        let at = (bottom_up * usize::from(pointer.width) + usize::from(column)) * 4;
        let [b, g, r, a] = [
            pointer.data[at],
            pointer.data[at + 1],
            pointer.data[at + 2],
            pointer.data[at + 3],
        ];
        assert_eq!((b, a), (0x5A, 0xC3), "blue and alpha at ({column}, {row})");
        (r, g)
    }

    fn shape(width: u16, height: u16) -> ChannelEvent {
        ChannelEvent::CursorShape(coordinate_image(width, height, (0, 0)))
    }

    fn visibility(visible: bool) -> ChannelEvent {
        ChannelEvent::CursorPosition {
            x: 10,
            y: 20,
            visible,
        }
    }

    #[test]
    fn a_pointer_is_bottom_up_bgra_with_straight_alpha() {
        // Top row red then green, bottom row blue then translucent
        // white.
        let pixels = vec![
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, //
            0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x40,
        ];
        let pointer = rdp_pointer(&image(2, 2, (1, 0), pixels)).expect("a pointer");

        assert_eq!(
            (pointer.cache_index, pointer.width, pointer.height),
            (0, 2, 2)
        );
        assert_eq!((pointer.hot_x, pointer.hot_y), (1, 0));
        assert_eq!(
            pointer.data,
            vec![
                0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x40, //
                0x00, 0x00, 0xFF, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
            ]
        );
    }

    #[test]
    fn a_wide_cursor_is_scaled_to_fit_keeping_its_aspect_ratio() {
        let pointer = rdp_pointer(&coordinate_image(200, 100, (150, 60))).expect("a pointer");

        assert_eq!((pointer.width, pointer.height), (96, 48));
        assert_eq!(pointer.data.len(), 96 * 48 * 4);
        assert_eq!((pointer.hot_x, pointer.hot_y), (72, 28));
        // Nearest neighbour: each pointer pixel samples the source at
        // the same proportion across and down.
        assert_eq!(source_of(&pointer, 0, 0), (0, 0));
        assert_eq!(source_of(&pointer, 48, 24), (100, 50));
        assert_eq!(source_of(&pointer, 95, 47), (197, 97));
    }

    #[test]
    fn a_tall_cursor_is_scaled_to_fit_keeping_its_aspect_ratio() {
        let pointer = rdp_pointer(&coordinate_image(50, 300, (49, 299))).expect("a pointer");

        assert_eq!((pointer.width, pointer.height), (16, 96));
        assert_eq!((pointer.hot_x, pointer.hot_y), (15, 95));
        assert_eq!(source_of(&pointer, 8, 48), (25, 150));
    }

    #[test]
    fn a_cursor_of_the_largest_size_is_not_scaled() {
        let pointer = rdp_pointer(&coordinate_image(96, 96, (40, 90))).expect("a pointer");

        assert_eq!((pointer.width, pointer.height), (96, 96));
        assert_eq!((pointer.hot_x, pointer.hot_y), (40, 90));
        for (column, row) in [(0, 0), (95, 0), (0, 95), (17, 63), (95, 95)] {
            assert_eq!(source_of(&pointer, column, row), (column as u8, row as u8));
        }
    }

    #[test]
    fn a_hot_spot_outside_the_cursor_is_clamped_inside_it() {
        let pointer = rdp_pointer(&coordinate_image(4, 3, (9, 7))).expect("a pointer");
        assert_eq!((pointer.hot_x, pointer.hot_y), (3, 2));

        let pointer = rdp_pointer(&coordinate_image(200, 100, (500, 400))).expect("a pointer");
        assert_eq!((pointer.hot_x, pointer.hot_y), (95, 47));
    }

    #[test]
    fn a_cursor_without_pixels_yields_no_pointer() {
        assert!(rdp_pointer(&image(0, 4, (0, 0), Vec::new())).is_none());
        assert!(rdp_pointer(&image(4, 0, (0, 0), Vec::new())).is_none());
        // Fewer pixels than the size says.
        assert!(rdp_pointer(&image(2, 2, (0, 0), vec![0; 15])).is_none());
    }

    #[test]
    fn a_shape_replaces_the_shape_and_keeps_the_visibility() {
        let (tx, mut rx) = watch::channel(CursorState::default());

        apply_event(&tx, shape(4, 3));
        assert!(rx.has_changed().expect("open"));
        let state = rx.borrow_and_update().clone();
        let image = state.shape.expect("a shape");
        assert_eq!((image.width, image.height), (4, 3));
        assert!(state.visible);
    }

    #[test]
    fn hide_and_show_change_only_the_visibility() {
        let (tx, mut rx) = watch::channel(CursorState::default());
        apply_event(&tx, shape(4, 3));
        rx.borrow_and_update();

        apply_event(&tx, visibility(false));
        assert!(rx.has_changed().expect("open"));
        let state = rx.borrow_and_update().clone();
        assert!(!state.visible);
        assert!(state.shape.is_some(), "a hidden cursor keeps its shape");

        apply_event(&tx, visibility(true));
        assert!(rx.has_changed().expect("open"));
        let state = rx.borrow_and_update().clone();
        assert!(state.visible);
        assert_eq!(state.shape.expect("a shape").width, 4);
    }

    #[test]
    fn a_move_that_changes_nothing_wakes_no_one() {
        let (tx, mut rx) = watch::channel(CursorState::default());

        apply_event(&tx, visibility(true));
        assert!(!rx.has_changed().expect("open"));

        apply_event(&tx, visibility(false));
        rx.borrow_and_update();
        apply_event(&tx, visibility(false));
        assert!(!rx.has_changed().expect("open"));

        apply_event(
            &tx,
            ChannelEvent::MonitorsConfig {
                width: 800,
                height: 600,
            },
        );
        assert!(!rx.has_changed().expect("open"));
    }

    #[tokio::test]
    async fn the_relay_carries_on_past_a_lag_and_ends_with_the_bus() {
        let (events_tx, events_rx) = broadcast::channel(2);
        let (rx, task) = spawn_cursor_relay(events_rx);

        // On the current-thread test runtime the relay has not run
        // yet, so four events into a bus of two overflow it.
        events_tx.send(shape(4, 3)).expect("send");
        events_tx.send(visibility(false)).expect("send");
        events_tx.send(visibility(true)).expect("send");
        events_tx.send(shape(8, 5)).expect("send");
        drop(events_tx);
        task.await.expect("the relay ends when the bus closes");

        let state = rx.borrow().clone();
        assert!(state.visible);
        let image = state.shape.expect("a shape");
        assert_eq!((image.width, image.height), (8, 5));
    }
}
