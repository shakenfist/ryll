//! Event bus: fans the renderer's `ChannelEvent` mpsc out to many
//! consumers and keeps a [`SurfaceMirror`] up to date from it.
//!
//! Every mode wires the same pipeline. A forwarder task drains the
//! renderer's mpsc and republishes each event onto a broadcast bus,
//! so several consumers (the headless stats drain, control-socket
//! clients, the web cursor relay and audio sink, digest consumers) can
//! tap the same stream without back-pressuring the SPICE channel
//! producers. A second task subscribes to the bus and pipes every
//! event through [`SurfaceMirror::apply_event`] on the shared mirror.
//! [`EventBus`] owns both so the frontends do not each hand-write them.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::channels::ChannelEvent;
use crate::surface_mirror::SurfaceMirror;

/// The tasks [`EventBus::start`] spawned, for a caller that needs to
/// abort or join them on teardown.
pub struct EventBusHandles {
    /// Drains the renderer's mpsc into the broadcast bus. Exits when
    /// the renderer drops its sender on session shutdown.
    pub forwarder: JoinHandle<()>,
    /// Applies bus events to the surface mirror. Exits when the bus
    /// closes.
    pub mirror: JoinHandle<()>,
}

/// A broadcast bus plus the surface mirror fed from it.
pub struct EventBus {
    tx: broadcast::Sender<ChannelEvent>,
    mirror: Arc<Mutex<SurfaceMirror>>,
    // Subscribed in `new` so the mirror cannot miss an event sent
    // between construction and `start`.
    mirror_rx: broadcast::Receiver<ChannelEvent>,
}

impl EventBus {
    /// Create a bus whose broadcast channel holds `capacity` events,
    /// with an empty mirror already subscribed to it.
    pub fn new(capacity: usize) -> Self {
        let (tx, mirror_rx) = broadcast::channel(capacity);
        Self {
            tx,
            mirror: Arc::new(Mutex::new(SurfaceMirror::new())),
            mirror_rx,
        }
    }

    /// A sender onto the bus, for a producer that publishes events of
    /// its own (the digest poller, say) or a consumer that needs to
    /// subscribe later (a control socket).
    ///
    /// A clone kept alive keeps the bus open: the mirror task and any
    /// other receiver only see the bus close once the forwarder and
    /// every clone handed out here have been dropped.
    pub fn sender(&self) -> broadcast::Sender<ChannelEvent> {
        self.tx.clone()
    }

    /// Subscribe to the bus. A receiver only sees events sent after it
    /// was created, so a consumer of events that arrive during
    /// session-init (the first surface, the first cursor shape) must
    /// subscribe before the session that produces them is spawned.
    pub fn subscribe(&self) -> broadcast::Receiver<ChannelEvent> {
        self.tx.subscribe()
    }

    /// The surface mirror the bus keeps up to date.
    pub fn mirror(&self) -> Arc<Mutex<SurfaceMirror>> {
        self.mirror.clone()
    }

    /// Spawn the forwarder and the mirror-apply task. `label` names
    /// the mode ("web", "headless") in log lines.
    ///
    /// Takes `self` so the bus's own sender goes to the forwarder and
    /// no other copy outlives this call: once the forwarder exits,
    /// the broadcast closes and the remaining receivers see
    /// `RecvError::Closed`, provided the caller has dropped the
    /// senders it cloned with [`EventBus::sender`].
    pub fn start(
        self,
        mut event_rx: mpsc::Receiver<ChannelEvent>,
        label: &'static str,
    ) -> EventBusHandles {
        let Self {
            tx,
            mirror,
            mut mirror_rx,
        } = self;

        // Forwarder: `broadcast::Sender::send` is non-blocking. An
        // error means there are no current receivers, which is fine
        // for events nobody is waiting on (the sender re-arms
        // automatically when subscribers appear).
        let forwarder = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let _ = tx.send(event);
            }
        });

        // Surface-mirror apply task. Every code path calls the same
        // `SurfaceMirror::apply_event` helper directly on the shared
        // mirror, so the apply dispatch is single-sourced inside
        // `surface_mirror.rs` itself. A `Lagged` error means a slow
        // consumer missed N events, so the mirror's pixels and damage
        // have diverged from what SPICE sent: mark every surface
        // fully damaged so downstream consumers resend whole
        // surfaces, and carry on rather than tearing the session down.
        let mirror_task = tokio::spawn(async move {
            loop {
                match mirror_rx.recv().await {
                    Ok(event) => {
                        let mut m = mirror.lock().await;
                        m.apply_event(&event);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            "{}: surface mirror lagged by {} events; resending every surface in full",
                            label, n
                        );
                        mirror.lock().await.mark_all_damaged();
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("{}: surface mirror task exiting (broadcast closed)", label);
                        break;
                    }
                }
            }
        });

        EventBusHandles {
            forwarder,
            mirror: mirror_task,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DamageRect;

    fn image_ready(left: u32, top: u32, width: u32, height: u32) -> ChannelEvent {
        ChannelEvent::ImageReady {
            display_channel_id: 0,
            surface_id: 0,
            left,
            top,
            width,
            height,
            pixels: vec![255; (width * height * 4) as usize],
            image_id: 0,
            produced_at_secs: 0.0,
        }
    }

    fn surface_created(width: u32, height: u32) -> ChannelEvent {
        ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id: 0,
            width,
            height,
        }
    }

    #[tokio::test]
    async fn events_through_mpsc_reach_the_mirror() {
        let bus = EventBus::new(16);
        let mirror = bus.mirror();
        let (tx, rx) = mpsc::channel(8);
        let handles = bus.start(rx, "test");

        tx.send(surface_created(2, 2)).await.unwrap();
        tx.send(image_ready(0, 0, 2, 2)).await.unwrap();
        // Closing the mpsc ends the forwarder, which closes the bus
        // (nothing else holds a sender), which ends the mirror task.
        drop(tx);
        handles.forwarder.await.unwrap();
        handles.mirror.await.unwrap();

        let m = mirror.lock().await;
        let s = m.primary_surface().expect("surface created");
        assert_eq!(s.size(), (2, 2));
        assert_eq!(&s.pixels()[0..4], &[255, 255, 255, 255]);
    }

    #[tokio::test]
    async fn lagged_mirror_ends_fully_damaged() {
        // Capacity 1 and a mirror task held up on the lock: the flood
        // below overruns the bus, so the task's next receive is
        // `Lagged` however far it had got.
        let bus = EventBus::new(1);
        let mirror = bus.mirror();
        let (tx, rx) = mpsc::channel(16);

        // Start from a surface with no pending damage.
        {
            let mut m = mirror.lock().await;
            m.apply_event(&surface_created(4, 4));
            let s = m.primary_surface_mut().unwrap();
            s.take_damage();
            s.consume_dirty();
        }
        let handles = bus.start(rx, "test");

        let guard = mirror.lock().await;
        for _ in 0..10 {
            tx.send(image_ready(0, 0, 1, 1)).await.unwrap();
        }
        // The forwarder has moved every event onto the bus once the
        // mpsc is empty again.
        while tx.capacity() < tx.max_capacity() {
            tokio::task::yield_now().await;
        }
        drop(guard);
        drop(tx);
        handles.forwarder.await.unwrap();
        handles.mirror.await.unwrap();

        let m = mirror.lock().await;
        let s = m.primary_surface().unwrap();
        // Draws of 1x1 alone would leave a 1x1 damage rect.
        assert_eq!(s.damage().rects(), &[DamageRect::new(0, 0, 4, 4)]);
        assert!(s.is_dirty());
    }
}
