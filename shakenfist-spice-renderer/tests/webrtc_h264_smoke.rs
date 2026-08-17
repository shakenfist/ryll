//! Phase 2 step 2e pre-flight: confirm webrtc-rs's H.264 RTP
//! payloader accepts the NAL output our H264Encoder produces.
//!
//! The encoder emits Annex-B-framed NAL units. The webrtc-rs
//! H264Payloader expects raw NAL bodies (no start codes), so
//! this test strips the leading 4-byte start code from each
//! NAL before payloading.
//!
//! We depend on the standalone `rtp` crate (re-exported by the
//! umbrella `webrtc` crate as `webrtc::rtp`) to keep the dev-dep
//! footprint minimal: this test is just the packetiser, not a
//! full WebRTC stack.
//!
//! ## Payloader semantics worth knowing about
//!
//! Per RFC 6184 and the rtp crate's H264Payloader implementation:
//!
//! * SPS (NAL type 7) and PPS (NAL type 8) NALs do NOT produce
//!   their own RTP payloads when fed individually. The payloader
//!   stashes them internally and emits them as a STAP-A
//!   aggregation packet together with the *next* non-parameter
//!   NAL it sees (typically the IDR slice). So `payload(...)`
//!   returns `Ok(vec![])` for an SPS or PPS — that's correct
//!   behaviour, not a failure.
//! * AUD (type 9) and filler (type 12) NALs are silently dropped
//!   and produce no payloads.
//! * For everything else (slices, IDR, etc.), the payloader
//!   either emits a single-NAL-unit packet (NAL ≤ MTU) or
//!   FU-A fragments (NAL > MTU). When SPS+PPS are stashed, the
//!   first non-parameter NAL produces *two* payloads: one
//!   STAP-A with SPS+PPS, then the NAL itself.
//!
//! ## Pass / fail signal
//!
//! If this test passes, Phase 2 ships H.264 by default. If it
//! fails (e.g. the payloader returns an error or fails to
//! bundle SPS+PPS into a STAP-A on the IDR), the contingency
//! is to swap to VP8 via vpx-encode in a follow-up commit; see
//! docs/plans/PLAN-web-frontend-phase-02-encoder.md, Approach
//! section "VP8 contingency pre-flight (step 2e)".

use bytes::Bytes;
use rtp::codecs::h264::{H264Payloader, FUA_NALU_TYPE, PPS_NALU_TYPE, SPS_NALU_TYPE};
use rtp::packetizer::Payloader;
use shakenfist_spice_renderer::{FrameSource, H264Encoder, SyntheticFrameSource};

/// MTU to hand to the payloader. 1200 is the conventional WebRTC
/// MTU (after IP/UDP/SRTP overhead from a 1500-byte ethernet
/// frame); large NALs would be FU-A fragmented to fit, though for
/// 64x64 frames every NAL is well under MTU.
const MTU: usize = 1200;

/// STAP-A aggregation packet NAL type (RFC 6184 §5.7.1).
const STAPA_NALU_TYPE: u8 = 24;

#[test]
fn webrtc_h264_payloader_accepts_encoder_output() {
    let mut encoder = H264Encoder::new(64, 64, 30).expect("encoder init");
    let mut source = SyntheticFrameSource::new(64, 64);

    // Encode 3 frames: the first will be an IDR (with SPS/PPS),
    // the next two will be P-frames.
    let mut frames = Vec::new();
    for _ in 0..3 {
        let frame_ref = source.next_frame().expect("source produces frames");
        let encoded = encoder.encode(frame_ref.rgba, false).expect("encode");
        frames.push(encoded);
    }

    assert!(frames[0].keyframe, "expected frame 0 to be keyframe");
    assert!(!frames[1].keyframe, "expected frame 1 to not be keyframe");

    let mut payloader = H264Payloader::default();

    // Cumulative counters across all 3 frames.
    let mut total_payloads = 0usize;
    let mut total_nals = 0usize;
    let mut idr_saw_sps = false;
    let mut idr_saw_pps = false;
    let mut idr_produced_stap_a = false;
    let mut nal_type_counts: std::collections::BTreeMap<u8, usize> =
        std::collections::BTreeMap::new();
    let mut payload_type_counts: std::collections::BTreeMap<u8, usize> =
        std::collections::BTreeMap::new();

    for (frame_idx, frame) in frames.iter().enumerate() {
        for nal in &frame.nal_units {
            // Annex-B framing: strip the 4-byte start code. The
            // encoder normalises everything to 4-byte start codes,
            // so a 3-byte prefix here would be a regression.
            assert!(
                nal.len() > 4,
                "NAL too short to contain a body: {} bytes",
                nal.len()
            );
            assert_eq!(
                &nal[0..4],
                &[0x00, 0x00, 0x00, 0x01],
                "expected Annex-B 4-byte start code"
            );
            let raw_nal = &nal[4..];
            let nal_type = raw_nal[0] & 0x1F;
            *nal_type_counts.entry(nal_type).or_insert(0) += 1;
            total_nals += 1;

            // Track SPS/PPS appearance on the IDR frame.
            if frame_idx == 0 {
                if nal_type == SPS_NALU_TYPE {
                    idr_saw_sps = true;
                }
                if nal_type == PPS_NALU_TYPE {
                    idr_saw_pps = true;
                }
            }

            // Payload one NAL.
            let bytes = Bytes::copy_from_slice(raw_nal);
            let payloads = payloader.payload(MTU, &bytes).unwrap_or_else(|e| {
                panic!(
                    "payloader rejected NAL type={} (frame {}, len {}): {}",
                    nal_type,
                    frame_idx,
                    raw_nal.len(),
                    e
                )
            });

            // Per-NAL expectation:
            //   * SPS / PPS produce zero payloads (cached for STAP-A).
            //   * Everything else must produce at least one
            //     non-empty payload.
            if nal_type == SPS_NALU_TYPE || nal_type == PPS_NALU_TYPE {
                assert!(
                    payloads.is_empty(),
                    "SPS/PPS should be cached and produce no payloads, got {} \
                     (frame {}, NAL type {})",
                    payloads.len(),
                    frame_idx,
                    nal_type
                );
            } else {
                assert!(
                    !payloads.is_empty(),
                    "payloader returned empty Vec for NAL type={} (frame {}, len {})",
                    nal_type,
                    frame_idx,
                    raw_nal.len()
                );
            }

            for p in &payloads {
                assert!(!p.is_empty(), "payloader produced empty payload");
                let payload_type = p[0] & 0x1F;
                *payload_type_counts.entry(payload_type).or_insert(0) += 1;

                // FU-A start packets must have the start bit set; we
                // don't expect FU-A here at 64x64 with MTU 1200 but if
                // it does happen, sanity-check the framing.
                if payload_type == FUA_NALU_TYPE {
                    assert!(p.len() >= 2, "FU-A packet too short");
                }

                // STAP-A appears only on the frame that flushes the
                // cached SPS+PPS, which is the IDR frame here.
                if payload_type == STAPA_NALU_TYPE && frame_idx == 0 {
                    idr_produced_stap_a = true;
                }
            }
            total_payloads += payloads.len();
        }
    }

    assert!(
        idr_saw_sps,
        "IDR frame did not contain an SPS NAL — encoder output may be malformed (saw types: {:?})",
        nal_type_counts
    );
    assert!(
        idr_saw_pps,
        "IDR frame did not contain a PPS NAL — encoder output may be malformed (saw types: {:?})",
        nal_type_counts
    );
    assert!(
        idr_produced_stap_a,
        "expected the IDR frame to produce a STAP-A packet bundling SPS+PPS, but no STAP-A \
         appeared (input NAL types: {:?}, output payload types: {:?})",
        nal_type_counts, payload_type_counts
    );
    assert!(total_payloads > 0, "no payloads produced across 3 frames");

    eprintln!(
        "webrtc_h264_smoke: payloaded {} NALs from 3 frames into {} RTP payloads \
         (input NAL types: {:?}; output payload types: {:?})",
        total_nals, total_payloads, nal_type_counts, payload_type_counts
    );
}
