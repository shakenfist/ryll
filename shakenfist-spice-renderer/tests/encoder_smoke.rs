//! End-to-end smoke test for the encoder pipeline.
//!
//! Spawns an `EncoderTask` driven by a `SyntheticFrameSource`,
//! collects the output Annex-B-framed NALs over ~3 seconds at
//! 30 fps, and writes them to `target/encoder_smoke.h264`.
//!
//! **Manual verification step:** after this test passes,
//! `ffplay target/encoder_smoke.h264` should play the
//! checkerboard-with-moving-band animation. The test itself
//! does not assert decode correctness — only that the encoder
//! produces a non-empty NAL stream with at least one keyframe
//! and roughly the expected number of frames. The visual check
//! is the human's job and confirms encoder output is genuinely
//! decodable.

use std::path::PathBuf;
use std::time::Duration;

use shakenfist_spice_renderer::{
    EncodedFrame, EncoderControl, EncoderTask, H264Encoder, SyntheticFrameSource,
};
use tokio::sync::mpsc;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 30;
const RUN_DURATION: Duration = Duration::from_secs(3);
/// Ideal frame count at 30 fps over 3 seconds.
const EXPECTED_FRAMES: usize = 90;
/// Minimum frames we require in a debug / Docker build where the
/// unoptimised encoder may run well below 30 fps. The floor is set
/// low enough to pass on slow CI while still verifying the pipeline
/// produced a meaningful NAL stream.
/// In release builds the encoder runs at full speed, so we require
/// at least 60 frames (~2 s of real output at 30 fps). This tighter
/// floor catches throughput regressions that a debug-build run
/// cannot, since software encoding under debug assertions is far
/// too slow to distinguish a pipeline stall from ordinary slowness.
const MIN_FRAMES: usize = if cfg!(debug_assertions) { 10 } else { 60 };

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoder_smoke_writes_playable_h264() {
    let encoder = H264Encoder::new(WIDTH, HEIGHT, FPS).expect("encoder init");
    let source = SyntheticFrameSource::new(WIDTH, HEIGHT);
    let (out_tx, mut out_rx) = mpsc::channel::<EncodedFrame>(64);
    let (ctl_tx, ctl_rx) = mpsc::channel::<EncoderControl>(4);

    let handle = EncoderTask::spawn(encoder, source, out_tx, ctl_rx, FPS);

    // Drain frames for ~3 seconds.
    let mut frames: Vec<EncodedFrame> = Vec::new();
    let deadline = tokio::time::Instant::now() + RUN_DURATION;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, out_rx.recv()).await {
            Ok(Some(f)) => frames.push(f),
            Ok(None) => break, // task ended
            Err(_) => break,   // deadline reached
        }
    }
    ctl_tx.send(EncoderControl::Stop).await.expect("send stop");
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // Sanity assertions.
    // In a release build at 30 fps we expect ~90 frames.  In an
    // unoptimised debug build inside Docker the encoder may be
    // slower than the frame budget, so we accept any count above
    // MIN_FRAMES and just report the actual count for information.
    assert!(
        frames.len() >= MIN_FRAMES,
        "expected at least {} frames, got {} (ideal: ~{})",
        MIN_FRAMES,
        frames.len(),
        EXPECTED_FRAMES
    );
    assert!(
        frames.iter().any(|f| f.keyframe),
        "expected at least one keyframe in {} frames",
        frames.len()
    );

    // Concatenate Annex-B NALs to produce a raw .h264 file.
    let mut bytes: Vec<u8> = Vec::new();
    for frame in &frames {
        for nal in &frame.nal_units {
            bytes.extend_from_slice(nal);
        }
    }
    assert!(!bytes.is_empty(), "no encoded bytes");

    // Write to target/encoder_smoke.h264. The path is resolved
    // relative to the workspace root (CARGO_MANIFEST_DIR is the
    // renderer crate; go up one level for the workspace root).
    let workspace_target: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("target");
    std::fs::create_dir_all(&workspace_target).expect("create target dir");
    let out_path = workspace_target.join("encoder_smoke.h264");
    std::fs::write(&out_path, &bytes).expect("write h264 file");

    eprintln!(
        "encoder_smoke: wrote {} frames ({} bytes) to {}",
        frames.len(),
        bytes.len(),
        out_path.display()
    );
    eprintln!(
        "encoder_smoke: manual verification: ffplay {}",
        out_path.display()
    );
}
