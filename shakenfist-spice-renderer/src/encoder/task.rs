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
            // Self-heal on guest-initiated resize: when the surface
            // mirror grows or shrinks, the FrameRef carries the new
            // dimensions. `resize` is a no-op when they already
            // match; on a real change it rebuilds the inner
            // openh264 encoder so the next encoded frame starts a
            // fresh stream (SPS / PPS + IDR), letting the browser
            // decoder reconfigure without a WebRTC renegotiation.
            // Without this, every post-resize encode would bail on
            // the rgba.len() mismatch and the browser would see a
            // frozen frame until the next /offer.
            encoder.resize(frame.width, frame.height)?;
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
        let encoder = H264Encoder::new(64, 64).expect("init");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_keyframe_marks_next_frame_idr() {
        let encoder = H264Encoder::new(64, 64).expect("init");
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

    /// A FrameSource that switches dimensions partway through a
    /// stream, mirroring what a guest-initiated display resize
    /// looks like from the encoder's perspective. Used to verify
    /// the encoder auto-resizes on FrameRef dimension changes
    /// instead of bailing on the rgba.len() mismatch — the bug
    /// observed in test-session-008e where 1280x800 → 1440x900
    /// froze the browser stream.
    struct ResizingFrameSource {
        first_rgba: Vec<u8>,
        first_dims: (u32, u32),
        second_rgba: Vec<u8>,
        second_dims: (u32, u32),
        produced: u32,
        switch_after: u32,
        total: u32,
    }

    impl ResizingFrameSource {
        fn new(first: (u32, u32), second: (u32, u32), switch_after: u32, total: u32) -> Self {
            let first_len = (first.0 as usize) * (first.1 as usize) * 4;
            let second_len = (second.0 as usize) * (second.1 as usize) * 4;
            let mut first_rgba = vec![0u8; first_len];
            let mut second_rgba = vec![0u8; second_len];
            for chunk in first_rgba.chunks_exact_mut(4) {
                chunk[3] = 255;
            }
            for chunk in second_rgba.chunks_exact_mut(4) {
                chunk[3] = 255;
            }
            Self {
                first_rgba,
                first_dims: first,
                second_rgba,
                second_dims: second,
                produced: 0,
                switch_after,
                total,
            }
        }
    }

    impl FrameSource for ResizingFrameSource {
        fn next_frame(&mut self) -> Option<FrameRef<'_>> {
            if self.produced >= self.total {
                return None;
            }
            let i = self.produced;
            self.produced += 1;
            let timestamp_us = (i as u64) * 33_333;
            if i < self.switch_after {
                Some(FrameRef {
                    width: self.first_dims.0,
                    height: self.first_dims.1,
                    rgba: &self.first_rgba,
                    timestamp_us,
                })
            } else {
                Some(FrameRef {
                    width: self.second_dims.0,
                    height: self.second_dims.1,
                    rgba: &self.second_rgba,
                    timestamp_us,
                })
            }
        }
    }

    /// Guest-initiated mid-session resize: the encoder must
    /// auto-resize on the new dimensions and continue emitting
    /// frames, not bail with an rgba.len() mismatch. The first
    /// frame after the resize must be an IDR (openh264 emits one
    /// implicitly as the first frame of the new inner encoder)
    /// so the browser decoder can reconfigure cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn encoder_auto_resizes_mid_stream() {
        // 3 frames at 64x64, then 5 frames at 96x96.
        let encoder = H264Encoder::new(64, 64).expect("init");
        let source = ResizingFrameSource::new((64, 64), (96, 96), 3, 8);
        let (tx, mut rx) = mpsc::channel(16);
        let (ctl_tx, ctl_rx) = mpsc::channel(4);

        let handle = EncoderTask::spawn(encoder, source, tx, ctl_rx, 60);

        let mut frames = Vec::new();
        let drain = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(f) = rx.recv().await {
                frames.push(f);
                if frames.len() == 8 {
                    break;
                }
            }
        })
        .await;
        assert!(drain.is_ok(), "drain timed out — encoder likely bailed");
        ctl_tx.send(EncoderControl::Stop).await.ok();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;

        assert_eq!(frames.len(), 8, "all 8 frames must encode across resize");
        assert!(frames[0].keyframe, "first frame is the implicit IDR");
        // Frame index 3 is the first frame at the new resolution;
        // the rebuilt inner encoder treats it as its own first
        // frame, so it must also be an IDR for the browser decoder
        // to reconfigure cleanly.
        assert!(
            frames[3].keyframe,
            "post-resize frame must be an IDR (got keyframe={})",
            frames[3].keyframe,
        );
    }

    /// `H264Encoder::resize` is a no-op when dimensions match. The
    /// inner encoder must stay the same so we don't drop frame
    /// state every tick.
    #[test]
    fn encoder_resize_is_noop_when_dims_match() {
        let mut encoder = H264Encoder::new(64, 64).expect("init");
        // Round-down behaviour: 65 → 64, must still be a no-op.
        encoder.resize(65, 65).expect("resize");
        assert_eq!(encoder.width(), 64);
        assert_eq!(encoder.height(), 64);
        encoder.resize(64, 64).expect("resize exact");
        assert_eq!(encoder.width(), 64);
        assert_eq!(encoder.height(), 64);
    }

    /// `H264Encoder::resize` rejects sub-2-pixel dimensions the
    /// same way `H264Encoder::new` does, so a buggy caller can't
    /// silently put the encoder into a 0x0 state.
    #[test]
    fn encoder_resize_rejects_tiny_dimensions() {
        let mut encoder = H264Encoder::new(64, 64).expect("init");
        let err = encoder.resize(1, 64).expect_err("must error on width=1");
        assert!(err.to_string().contains("too small"));
        let err = encoder.resize(64, 1).expect_err("must error on height=1");
        assert!(err.to_string().contains("too small"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_terminates_task() {
        let encoder = H264Encoder::new(64, 64).expect("init");
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
        let encoder = H264Encoder::new(64, 64).expect("init");
        let source = CountedFrameSource::new(64, 64, 1);
        let (tx, _rx) = mpsc::channel(1);
        let (_ctl_tx, ctl_rx) = mpsc::channel(1);
        let res = run(encoder, source, tx, ctl_rx, 0);
        assert!(res.is_err());
    }
}
