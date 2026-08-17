//! Async driver for the H.264 encoder: configurable FPS cap,
//! encode-on-frame-availability, keyframe-on-demand.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use super::frame_source::FrameSource;
use super::h264::{EncodedFrame, H264Encoder};

/// Control messages sent to a running [`EncoderTask`].
#[derive(Debug)]
pub enum EncoderControl {
    /// Force the next encoded frame to be an IDR keyframe.
    /// Phase 3+ calls this whenever a new viewer attaches.
    RequestKeyframe,
    /// Stop the task. The task returns `Ok(())` after the
    /// current encode (if any) completes.
    Stop,
}

/// Async driver around an [`H264Encoder`]. Spawned via
/// [`EncoderTask::spawn`], which returns a [`tokio::task::JoinHandle`]
/// that resolves when the task stops (either via
/// [`EncoderControl::Stop`] or because the output channel's
/// receiver is dropped). `next_frame` returning `None` is
/// **not** a stop condition — only an explicit Stop or a
/// channel error stops the task.
pub struct EncoderTask;

impl EncoderTask {
    /// Spawn the task on tokio's blocking pool.
    ///
    /// # Parameters
    ///
    /// - `encoder`: the [`H264Encoder`] to drive.
    /// - `source`: pixel source polled once per tick.
    /// - `output`: channel on which [`EncodedFrame`]s are sent;
    ///   backpressure is applied via blocking send — the task
    ///   will stall if the receiver is slow.
    /// - `control`: receives [`EncoderControl`] messages; checked
    ///   at the start of every tick (non-blocking).
    /// - `fps_cap`: maximum frames per second; must be > 0.
    pub fn spawn<S: FrameSource + Send + 'static>(
        encoder: H264Encoder,
        source: S,
        output: mpsc::Sender<EncodedFrame>,
        control: mpsc::Receiver<EncoderControl>,
        fps_cap: u32,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::task::spawn_blocking(move || run(encoder, source, output, control, fps_cap))
    }
}

fn run<S: FrameSource>(
    mut encoder: H264Encoder,
    mut source: S,
    output: mpsc::Sender<EncodedFrame>,
    mut control: mpsc::Receiver<EncoderControl>,
    fps_cap: u32,
) -> Result<()> {
    if fps_cap == 0 {
        anyhow::bail!("EncoderTask: fps_cap must be > 0");
    }
    let frame_period = Duration::from_micros(1_000_000 / fps_cap as u64);
    let mut keyframe_pending = false;

    loop {
        let tick_start = Instant::now();

        // Drain any pending control messages without blocking.
        loop {
            match control.try_recv() {
                Ok(EncoderControl::RequestKeyframe) => {
                    keyframe_pending = true;
                }
                Ok(EncoderControl::Stop) => return Ok(()),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Try to pull a fresh frame and encode it.
        if let Some(frame) = source.next_frame() {
            // The guest can change resolution mid-session — the
            // browser's viewport message drives vdagent, which
            // resizes the primary surface underneath us. The
            // encoder is built for one size and rejects a buffer
            // of any other, so without this the first frame after
            // a resize kills the task and the viewer's video
            // freezes until the next offer.
            //
            // Rebuilding costs an IDR, which is the same price a
            // resolution change costs anywhere else, and browsers
            // take the new SPS/PPS in stride.
            if frame.width != encoder.width() || frame.height != encoder.height() {
                tracing::info!(
                    "EncoderTask: surface resized {}x{} -> {}x{}; rebuilding encoder",
                    encoder.width(),
                    encoder.height(),
                    frame.width,
                    frame.height
                );
                encoder = H264Encoder::new(frame.width, frame.height, fps_cap)?;
                keyframe_pending = true;
            }

            let force_kf = keyframe_pending;
            match encoder.encode(frame.rgba, force_kf) {
                Ok(mut encoded) => {
                    encoded.timestamp_us = frame.timestamp_us;
                    if force_kf {
                        keyframe_pending = false;
                    }
                    // blocking_send provides backpressure: if the receiver
                    // is slow the task stalls rather than dropping frames.
                    if output.blocking_send(encoded).is_err() {
                        // Receiver dropped; nothing left to do.
                        return Ok(());
                    }
                }
                Err(e) => {
                    // Encoder error — propagate. The caller decides
                    // whether to recreate the encoder.
                    return Err(e);
                }
            }
        }

        // Sleep the remainder of the frame budget. If encoding took
        // longer than the budget, skip rather than catch up.
        let elapsed = tick_start.elapsed();
        if elapsed < frame_period {
            std::thread::sleep(frame_period - elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{FrameRef, FrameSource, H264Encoder};

    /// A FrameSource that yields N pre-built RGBA frames then None
    /// forever. Each frame has timestamp_us = i * 33_333.
    struct CountedFrameSource {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        remaining: u32,
        produced: u32,
    }

    impl CountedFrameSource {
        fn new(width: u32, height: u32, count: u32) -> Self {
            let n = (width as usize) * (height as usize);
            let mut rgba = vec![0u8; n * 4];
            for i in 0..n {
                rgba[i * 4 + 3] = 255;
            }
            Self {
                rgba,
                width,
                height,
                remaining: count,
                produced: 0,
            }
        }
    }

    impl FrameSource for CountedFrameSource {
        fn next_frame(&mut self) -> Option<FrameRef<'_>> {
            if self.remaining == 0 {
                return None;
            }
            self.remaining -= 1;
            let timestamp_us = (self.produced as u64) * 33_333;
            self.produced += 1;
            Some(FrameRef {
                width: self.width,
                height: self.height,
                rgba: &self.rgba,
                timestamp_us,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_emits_frames_with_propagated_timestamps() {
        let encoder = H264Encoder::new(64, 64, 30).expect("init");
        let source = CountedFrameSource::new(64, 64, 5);
        let (tx, mut rx) = mpsc::channel(16);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);

        // 60 fps → 16ms tick; 5 frames takes ~80ms.
        let handle = EncoderTask::spawn(encoder, source, tx, ctl_rx, 60);

        // Drain frames until source is exhausted; we have to send Stop
        // because next_frame returning None doesn't stop the task.
        let mut frames = Vec::new();
        let drain = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(frame) = rx.recv().await {
                frames.push(frame);
                if frames.len() == 5 {
                    break;
                }
            }
        })
        .await;
        assert!(drain.is_ok(), "drain timed out");
        ctl_tx.send(EncoderControl::Stop).await.expect("send stop");

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task didn't stop in time")
            .expect("join")
            .expect("task returned error");

        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].timestamp_us, 0);
        assert_eq!(frames[1].timestamp_us, 33_333);
        assert!(frames[0].keyframe, "first frame should be a keyframe");
    }

    /// Yields `first` frames at one size then switches to another,
    /// as the surface mirror does when vdagent resizes the guest.
    struct ResizingFrameSource {
        small: Vec<u8>,
        large: Vec<u8>,
        produced: u32,
        switch_after: u32,
    }

    impl FrameSource for ResizingFrameSource {
        fn next_frame(&mut self) -> Option<FrameRef<'_>> {
            let timestamp_us = (self.produced as u64) * 33_333;
            let small = self.produced < self.switch_after;
            self.produced += 1;
            Some(if small {
                FrameRef {
                    width: 64,
                    height: 64,
                    rgba: &self.small,
                    timestamp_us,
                }
            } else {
                FrameRef {
                    width: 32,
                    height: 32,
                    rgba: &self.large,
                    timestamp_us,
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mid_session_resize_rebuilds_the_encoder() {
        // Before this was handled, the first frame at the new size
        // failed H264Encoder::encode's length check, the task
        // returned Err, and the viewer's video froze until the
        // next offer restarted the pipeline.
        let mut small = vec![0u8; 64 * 64 * 4];
        for i in 0..(64 * 64) {
            small[i * 4 + 3] = 255;
        }
        let mut large = vec![0u8; 32 * 32 * 4];
        for i in 0..(32 * 32) {
            large[i * 4 + 3] = 255;
        }

        let encoder = H264Encoder::new(64, 64, 30).expect("init");
        let source = ResizingFrameSource {
            small,
            large,
            produced: 0,
            switch_after: 2,
        };
        let (tx, mut rx) = mpsc::channel(16);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);
        let handle = EncoderTask::spawn(encoder, source, tx, ctl_rx, 60);

        let mut frames = Vec::new();
        let drain = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(frame) = rx.recv().await {
                frames.push(frame);
                if frames.len() == 4 {
                    break;
                }
            }
        })
        .await;
        assert!(drain.is_ok(), "timed out draining frames");
        // The drain loop also ends when the sender is dropped, so
        // count the frames rather than trust that it finished: an
        // encoder that dies at the resize closes the channel, and
        // the loop then exits cleanly having collected too few.
        assert_eq!(
            frames.len(),
            4,
            "encoder stopped producing across the resize"
        );
        ctl_tx.send(EncoderControl::Stop).await.expect("send stop");

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task didn't stop in time")
            .expect("join")
            .expect("task returned error across the resize");

        // The first frame at the new size must be an IDR: the
        // decoder needs the new SPS/PPS before it can use anything
        // that follows.
        assert!(
            frames[2].keyframe,
            "first frame after the resize should be a keyframe"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_keyframe_marks_next_frame_idr() {
        let encoder = H264Encoder::new(64, 64, 30).expect("init");
        let source = CountedFrameSource::new(64, 64, 10);
        let (tx, mut rx) = mpsc::channel(16);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);

        let handle = EncoderTask::spawn(encoder, source, tx, ctl_rx, 60);

        // Receive first frame (will be a keyframe by default).
        let first = rx.recv().await.expect("first frame");
        assert!(first.keyframe);

        // Receive a few P-frames.
        for _ in 0..3 {
            let f = rx.recv().await.expect("p frame");
            assert!(!f.keyframe);
        }

        // Request keyframe.
        ctl_tx
            .send(EncoderControl::RequestKeyframe)
            .await
            .expect("send req");

        // The next frame the task encodes should be a keyframe.
        // Because of timing, we may need to drain up to a couple of
        // already-in-flight P-frames before the keyframe arrives.
        let mut saw_keyframe = false;
        for _ in 0..3 {
            let f = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("recv timed out")
                .expect("recv");
            if f.keyframe {
                saw_keyframe = true;
                break;
            }
        }
        assert!(
            saw_keyframe,
            "expected a keyframe within 3 frames of RequestKeyframe"
        );

        ctl_tx.send(EncoderControl::Stop).await.expect("send stop");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_terminates_task() {
        let encoder = H264Encoder::new(64, 64, 30).expect("init");
        let source = CountedFrameSource::new(64, 64, 1000);
        let (tx, _rx) = mpsc::channel(16);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);

        let handle = EncoderTask::spawn(encoder, source, tx, ctl_rx, 30);

        // Let it run briefly then stop.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        ctl_tx.send(EncoderControl::Stop).await.expect("send stop");

        let res = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task didn't stop in time")
            .expect("join");
        assert!(res.is_ok());
    }

    #[test]
    fn rejects_zero_fps() {
        // Run the inner `run` directly to sidestep the tokio runtime.
        let encoder = H264Encoder::new(64, 64, 30).expect("init");
        let source = CountedFrameSource::new(64, 64, 1);
        let (tx, _rx) = mpsc::channel(1);
        let (_ctl_tx, ctl_rx) = mpsc::channel(1);
        let res = run(encoder, source, tx, ctl_rx, 0);
        assert!(res.is_err());
    }
}
