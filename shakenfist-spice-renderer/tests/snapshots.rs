//! Tests pinning the JSON field names that a ryll bug report's
//! `channel-state.json` exposes.
//!
//! These moved here from `ryll/src/bugreport.rs` because the types they
//! test are defined in `src/snapshots.rs`.

use serde_json::json;
use shakenfist_spice_protocol::link::H264Offer;
use shakenfist_spice_renderer::snapshots::{
    ChannelSnapshots, CursorCacheEntry, CursorSnapshot, DecodeResult, DisplaySnapshot,
    InputEventRecord, InputsSnapshot, MainSnapshot, OutstandingResizeSnapshot, PlaybackCodec,
    PlaybackSessionInfo, PlaybackSnapshot, RedirectedDevice, StreamSnapshot, UsbredirSnapshot,
    WebdavSnapshot,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one fully populated DisplaySnapshot, built field by field so \
     that serialising it pins down every name a bug report exposes, \
     the per-stream and per-codec counters included. The assertions \
     only mean anything read against the fixture they came from, and \
     splitting them into several tests would duplicate the fixture, \
     which is the thing under test"
)]
fn test_display_snapshot_serialises() {
    let mut snap = DisplaySnapshot {
        image_cache_entries: 3,
        image_cache_ids: vec![1, 2, 3],
        image_cache_bytes: 12345,
        bytes_in: 100_000,
        // "Video not keeping up" diagnostic fields.
        decode_total_count: 7,
        decode_failed_count: 1,
        decode_from_cache_count: 2,
        decode_recent_min_us: 250,
        decode_recent_max_us: 9000,
        decode_recent_mean_us: 1500,
        socket_read_count: 42,
        socket_reads_at_chunk_cap: 5,
        socket_max_chunk_bytes: 262_144,
        ack_send_count: 3,
        last_ack_send_ts_secs: Some(4.25),
        // Pcap writer-queue drop counter.
        writer_dropped_count: 11,
        // Link-up preference-message send markers.
        pref_compression_sent: true,
        pref_video_codec_type_sent: true,
        h264_offer: H264Offer::Preferred,
        ..Default::default()
    };
    snap.recent_decodes.push_back(DecodeResult {
        image_type: "GlzRgb".to_string(),
        image_id: 42,
        width: 800,
        height: 600,
        from_cache: false,
        success: true,
        timestamp_secs: 1.5,
        decode_duration_us: 1234,
    });
    snap.recent_ack_intervals_secs.push_back(0.42);
    // Stream-diagnostics fields: one active MJPEG stream
    // exercising every per-stream counter so the
    // serialiser is locked in to the field names a bug
    // report will expose.
    snap.streams_active.push(StreamSnapshot {
        stream_id: 38,
        surface_id: 0,
        codec_type: 1,
        stream_width: 1600,
        stream_height: 1200,
        dest_top: 0,
        dest_left: 0,
        dest_bottom: 1200,
        dest_right: 1600,
        created_at_secs: 12.5,
        frames_received: 240,
        frames_decoded_ok: 200,
        frames_decode_failed: 40,
        frames_dropped_empty_dest: 0,
        last_frame_ts_secs: Some(45.5),
        last_decode_ok_ts_secs: Some(45.4),
        last_decode_duration_us: 18_321,
        destroyed_at_secs: None,
        report_is_active: true,
        report_unique_id: 0xDEAD_BEEF,
        report_max_window_size: 5,
        report_timeout_ms: 1000,
        report_send_count: 3,
        last_report_sent_ts_secs: Some(12.5),
        last_report_num_frames: 5,
        last_report_num_drops: 1,
        last_report_last_frame_delay: -42,
        mjpeg_decoder_backend: "jpeg-decoder".to_string(),
        video_decoder_backend: "jpeg-decoder".to_string(),
    });
    snap.streams_created_total = 2;
    snap.streams_destroyed_total = 1;
    snap.stream_data_orphan_count = 3;
    snap.stream_reports_sent_total = 17;
    snap.stream_reports_unsupported_signals_sent = 2;
    // Aggregate MJPEG decode duration fields.
    snap.mjpeg_decode_recent_min_us = 1_200;
    snap.mjpeg_decode_recent_max_us = 45_000;
    snap.mjpeg_decode_recent_mean_us = 8_500;
    snap.mjpeg_decode_total_count = 350;
    snap.mjpeg_decode_failed_count = 2;
    // Aggregate H.264 decode duration fields (same shape as the MJPEG
    // aggregates).
    snap.h264_decode_recent_min_us = 5_000;
    snap.h264_decode_recent_max_us = 28_000;
    snap.h264_decode_recent_mean_us = 12_500;
    snap.h264_decode_total_count = 120;
    snap.h264_decode_failed_count = 1;
    // Previously-active stream now in the recently-destroyed
    // ring: destroyed_at_secs is Some, counters are frozen.
    snap.streams_recently_destroyed.push_back(StreamSnapshot {
        stream_id: 37,
        surface_id: 0,
        codec_type: 1,
        stream_width: 1600,
        stream_height: 1200,
        dest_top: 0,
        dest_left: 0,
        dest_bottom: 1200,
        dest_right: 1600,
        created_at_secs: 4.0,
        frames_received: 30,
        frames_decoded_ok: 28,
        frames_decode_failed: 2,
        frames_dropped_empty_dest: 1,
        last_frame_ts_secs: Some(6.0),
        last_decode_ok_ts_secs: Some(6.0),
        last_decode_duration_us: 17_500,
        destroyed_at_secs: Some(6.25),
        report_is_active: false,
        report_unique_id: 0,
        report_max_window_size: 0,
        report_timeout_ms: 0,
        report_send_count: 0,
        last_report_sent_ts_secs: None,
        last_report_num_frames: 0,
        last_report_num_drops: 0,
        last_report_last_frame_delay: 0,
        mjpeg_decoder_backend: "jpeg-decoder".to_string(),
        video_decoder_backend: "jpeg-decoder".to_string(),
    });
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(v["image_cache_entries"], json!(3));
    assert_eq!(v["bytes_in"], json!(100_000));
    // Decode, socket-read and ack fields visible in
    // channel-state.json.
    assert_eq!(v["recent_decodes"][0]["image_type"], json!("GlzRgb"));
    assert_eq!(v["recent_decodes"][0]["decode_duration_us"], json!(1234));
    assert_eq!(v["decode_total_count"], json!(7));
    assert_eq!(v["decode_failed_count"], json!(1));
    assert_eq!(v["decode_from_cache_count"], json!(2));
    assert_eq!(v["decode_recent_min_us"], json!(250));
    assert_eq!(v["decode_recent_max_us"], json!(9000));
    assert_eq!(v["decode_recent_mean_us"], json!(1500));
    assert_eq!(v["socket_read_count"], json!(42));
    assert_eq!(v["socket_reads_at_chunk_cap"], json!(5));
    assert_eq!(v["socket_max_chunk_bytes"], json!(262_144));
    assert_eq!(v["ack_send_count"], json!(3));
    assert_eq!(v["last_ack_send_ts_secs"], json!(4.25));
    assert_eq!(v["recent_ack_intervals_secs"], json!([0.42]));
    // Pcap writer-queue drop counter.
    assert_eq!(v["writer_dropped_count"], json!(11));

    // Stream-diagnostics fields. The presence of these in the
    // serialised display channel state is what lets a bug
    // report answer "did MJPEG frames arrive / decode / paint?".
    //
    // Read out of the arrays by index rather than searched for
    // in the document text: the two streams below differ only in
    // their field *values*, so a substring check could not tell
    // which array a match came from, and passed as long as the
    // number appeared somewhere.
    let active = v["streams_active"]
        .as_array()
        .expect("streams_active must serialise as an array");
    assert_eq!(active.len(), 1);
    let stream = &active[0];
    assert_eq!(stream["stream_id"], json!(38));
    assert_eq!(stream["codec_type"], json!(1));
    assert_eq!(stream["stream_width"], json!(1600));
    assert_eq!(stream["frames_received"], json!(240));
    assert_eq!(stream["frames_decoded_ok"], json!(200));
    assert_eq!(stream["frames_decode_failed"], json!(40));
    // Frames decoded but never painted. Documented alongside the
    // two counters above, and the reason a live stream can read
    // `frames_decoded_ok: 0` without anything being broken.
    assert_eq!(stream["frames_dropped_empty_dest"], json!(0));
    assert_eq!(stream["last_frame_ts_secs"], json!(45.5));
    assert_eq!(stream["last_decode_ok_ts_secs"], json!(45.4));
    assert_eq!(stream["last_decode_duration_us"], json!(18_321));
    // An active stream carries no destroy timestamp.
    assert_eq!(stream["destroyed_at_secs"], json!(null));
    assert_eq!(v["streams_created_total"], json!(2));
    assert_eq!(v["streams_destroyed_total"], json!(1));
    assert_eq!(v["stream_data_orphan_count"], json!(3));

    // Recently-destroyed ring: counters survive teardown so a
    // bug report between flap cycles still answers "did MJPEG
    // decode during stream X's life?". destroyed_at_secs is
    // always Some for entries in the ring.
    let destroyed = v["streams_recently_destroyed"]
        .as_array()
        .expect("streams_recently_destroyed must serialise as an array");
    assert_eq!(destroyed.len(), 1);
    assert_eq!(destroyed[0]["stream_id"], json!(37));
    assert_eq!(destroyed[0]["destroyed_at_secs"], json!(6.25));
    assert_eq!(destroyed[0]["frames_dropped_empty_dest"], json!(1));

    // STREAM_ACTIVATE_REPORT fields: verify that
    // activation state and last-sent-report mirrors are
    // visible in channel-state.json.
    assert_eq!(stream["report_is_active"], json!(true));
    assert_eq!(stream["report_unique_id"], json!(3_735_928_559u64));
    assert_eq!(stream["report_max_window_size"], json!(5));
    assert_eq!(stream["report_timeout_ms"], json!(1000));
    assert_eq!(stream["report_send_count"], json!(3));
    assert_eq!(stream["last_report_sent_ts_secs"], json!(12.5));
    assert_eq!(stream["last_report_num_frames"], json!(5));
    assert_eq!(stream["last_report_num_drops"], json!(1));
    assert_eq!(stream["last_report_last_frame_delay"], json!(-42));
    assert_eq!(v["stream_reports_sent_total"], json!(17));
    assert_eq!(v["stream_reports_unsupported_signals_sent"], json!(2));

    // MJPEG decoder backend name visible in bug reports so a report
    // identifies which decode path ran.
    assert_eq!(stream["mjpeg_decoder_backend"], json!("jpeg-decoder"));
    // General-purpose video_decoder_backend field visible for every stream
    // regardless of codec. For MJPEG streams this matches mjpeg_decoder_backend;
    // for H.264 it would show "H264 (openh264)" while mjpeg_decoder_backend is
    // empty.
    assert_eq!(stream["video_decoder_backend"], json!("jpeg-decoder"));

    // Aggregate MJPEG decode duration fields.
    assert_eq!(v["mjpeg_decode_recent_min_us"], json!(1200));
    assert_eq!(v["mjpeg_decode_recent_max_us"], json!(45000));
    assert_eq!(v["mjpeg_decode_recent_mean_us"], json!(8500));
    assert_eq!(v["mjpeg_decode_total_count"], json!(350));
    assert_eq!(v["mjpeg_decode_failed_count"], json!(2));
    // Aggregate H.264 decode duration fields (same shape and naming
    // convention as the MJPEG aggregates).
    assert_eq!(v["h264_decode_recent_min_us"], json!(5000));
    assert_eq!(v["h264_decode_recent_max_us"], json!(28000));
    assert_eq!(v["h264_decode_recent_mean_us"], json!(12500));
    assert_eq!(v["h264_decode_total_count"], json!(120));
    assert_eq!(v["h264_decode_failed_count"], json!(1));
    // Link-up preference-message send markers must appear in
    // channel-state.json so a bug-report reader can confirm the client
    // sent its image compression and video codec preferences, and
    // whether H.264 was among them, without reading the pcap.
    assert_eq!(v["pref_compression_sent"], json!(true));
    assert_eq!(v["pref_video_codec_type_sent"], json!(true));
    assert_eq!(v["h264_offer"], json!("preferred"));
    // Bounded image-cache eviction and cap fields. These must appear in
    // bug reports so an operator can tell how much eviction pressure the
    // session experienced and what cap was in effect.
    snap.image_cache_evictions_total = 42;
    snap.image_cache_evicted_bytes_total = 441_450_496;
    // 256 MiB default cap.
    snap.image_cache_cap_bytes = 268_435_456;
    // GLZ-dictionary cache stats live in their own snapshot fields
    // rather than being summed into `image_cache_*`. A bug report must
    // surface both sets so an operator can tell which cache is under
    // pressure.
    snap.glz_dictionary_entries = 17;
    snap.glz_dictionary_bytes = 4_194_304;
    // 256 MiB default cap (matches the image-cache default).
    snap.glz_dictionary_cap_bytes = 268_435_456;
    snap.glz_dictionary_evictions_total = 5;
    snap.glz_dictionary_evicted_bytes_total = 1_048_576;
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let with_caps: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(with_caps["image_cache_evictions_total"], json!(42));
    assert_eq!(
        with_caps["image_cache_evicted_bytes_total"],
        json!(441_450_496u64)
    );
    assert_eq!(with_caps["image_cache_cap_bytes"], json!(268_435_456u64));
    assert_eq!(with_caps["glz_dictionary_entries"], json!(17));
    assert_eq!(with_caps["glz_dictionary_bytes"], json!(4_194_304u64));
    assert_eq!(with_caps["glz_dictionary_cap_bytes"], json!(268_435_456u64));
    assert_eq!(with_caps["glz_dictionary_evictions_total"], json!(5));
    assert_eq!(
        with_caps["glz_dictionary_evicted_bytes_total"],
        json!(1_048_576u64)
    );
}

#[test]
fn test_inputs_snapshot_serialises() {
    let mut snap = InputsSnapshot {
        button_state: 1,
        writer_dropped_count: 4,
        unknown_opcode_count: 3,
        last_unknown_opcode: Some(0xBEEF),
        ..Default::default()
    };
    snap.recent_events.push_back(InputEventRecord {
        event_type: "KeyDown".to_string(),
        scancode: 0x1E,
        x: 0,
        y: 0,
        button_mask: 0,
        timestamp_secs: 2.0,
    });
    snap.messages_recv_by_opcode.insert(101, 5);
    snap.messages_recv_by_opcode.insert(102, 2);
    snap.messages_send_by_opcode.insert(1, 10);
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_baseline_channel_fields(
        &v,
        4,
        3,
        48879,
        json!({"101": 5, "102": 2}),
        json!({"1": 10}),
    );
    assert_eq!(v["button_state"], json!(1));
    assert_eq!(v["recent_events"][0]["event_type"], json!("KeyDown"));
}

#[test]
fn test_cursor_snapshot_serialises() {
    let mut snap = CursorSnapshot {
        cache_entries: 1,
        writer_dropped_count: 9,
        unknown_opcode_count: 1,
        last_unknown_opcode: Some(0xFF),
        ..Default::default()
    };
    snap.cache_contents.push(CursorCacheEntry {
        cursor_id: 99,
        width: 24,
        height: 24,
        hot_spot_x: 0,
        hot_spot_y: 0,
    });
    snap.messages_recv_by_opcode.insert(200, 7);
    snap.messages_send_by_opcode.insert(3, 4);
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_baseline_channel_fields(&v, 9, 1, 255, json!({"200": 7}), json!({"3": 4}));
    assert_eq!(v["cache_entries"], json!(1));
    assert_eq!(v["cache_contents"][0]["cursor_id"], json!(99));
}

/// Assert the fields every per-channel snapshot carries: the
/// writer-drop counter, the unknown-opcode pair, and the two
/// opcode histograms compared as whole maps (serde renders their
/// integer keys as decimal strings).
///
/// Every per-channel serialisation test repeats this set, so it is
/// expressed once here.
///
/// Lookups are by field name. The substring assertions this
/// replaces were satisfied by the expected text appearing anywhere
/// in the document — `json.contains("32768")` passes on a stray
/// 32768 in an unrelated field, and every one of them breaks on a
/// cosmetic change to the pretty-printer. The pattern is the one
/// already used for `recent_agent_reply_lag_us` per PR #105 review
/// #2 item 9, generalised.
fn assert_baseline_channel_fields(
    v: &serde_json::Value,
    writer_dropped_count: u64,
    unknown_opcode_count: u64,
    last_unknown_opcode: u64,
    messages_recv_by_opcode: serde_json::Value,
    messages_send_by_opcode: serde_json::Value,
) {
    assert_eq!(v["writer_dropped_count"], json!(writer_dropped_count));
    assert_eq!(v["unknown_opcode_count"], json!(unknown_opcode_count));
    assert_eq!(v["last_unknown_opcode"], json!(last_unknown_opcode));
    assert_eq!(v["messages_recv_by_opcode"], messages_recv_by_opcode);
    assert_eq!(v["messages_send_by_opcode"], messages_send_by_opcode);
}

#[test]
fn test_main_snapshot_serialises() {
    let mut snap = MainSnapshot {
        session_id: Some(42),
        bytes_in: 500,
        bytes_out: 100,
        writer_dropped_count: 2,
        // STREAM_REPORT mm_time visibility.
        mm_time_now: 123_456,
        mm_time_set_count: 7,
        last_mm_time_set_ts_secs: Some(12.5),
        unknown_opcode_count: 2,
        last_unknown_opcode: Some(0x1234),
        // Vdagent reply-lag tracking.
        agent_request_count: 5,
        agent_reply_count: 4,
        agent_reply_error_count: 1,
        last_agent_reply_ts_secs: Some(30.5),
        last_agent_reply_lag_us: Some(850),
        outstanding_agent_request_count: 1,
        // Agent token starvation.
        agent_tokens: 3,
        queued_agent_message_count: 2,
        agent_starved_since_ts_secs: Some(41.25),
        agent_stall_count: 1,
        // Unapplied-resize detection.
        resize_requests_sent_count: 3,
        resize_unapplied_count: 1,
        outstanding_resize: Some(OutstandingResizeSnapshot {
            width: 1992,
            height: 1256,
            sent_at_secs: 52.5,
        }),
        ..Default::default()
    };
    snap.messages_recv_by_opcode.insert(10, 3);
    snap.messages_recv_by_opcode.insert(20, 1);
    snap.messages_send_by_opcode.insert(5, 8);
    snap.recent_agent_reply_lag_us.push_back(820);
    snap.recent_agent_reply_lag_us.push_back(850);
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_baseline_channel_fields(&v, 2, 2, 4660, json!({"10": 3, "20": 1}), json!({"5": 8}));

    assert_eq!(v["session_id"], json!(42));
    // STREAM_REPORT mm_time visibility.
    assert_eq!(v["mm_time_now"], json!(123_456));
    assert_eq!(v["mm_time_set_count"], json!(7));
    assert_eq!(v["last_mm_time_set_ts_secs"], json!(12.5));
    // Vdagent reply-lag fields.
    assert_eq!(v["agent_request_count"], json!(5));
    assert_eq!(v["agent_reply_count"], json!(4));
    assert_eq!(v["agent_reply_error_count"], json!(1));
    assert_eq!(v["last_agent_reply_ts_secs"], json!(30.5));
    assert_eq!(v["last_agent_reply_lag_us"], json!(850));
    assert_eq!(v["outstanding_agent_request_count"], json!(1));
    assert_eq!(v["recent_agent_reply_lag_us"], json!([820, 850]));
    // Agent token starvation fields.
    assert_eq!(v["agent_tokens"], json!(3));
    assert_eq!(v["queued_agent_message_count"], json!(2));
    assert_eq!(v["agent_starved_since_ts_secs"], json!(41.25));
    assert_eq!(v["agent_stall_count"], json!(1));
    // Unapplied-resize fields, as docs/troubleshooting.md names
    // them.
    assert_eq!(v["resize_requests_sent_count"], json!(3));
    assert_eq!(v["resize_unapplied_count"], json!(1));
    assert_eq!(v["outstanding_resize"]["width"], json!(1992));
    assert_eq!(v["outstanding_resize"]["height"], json!(1256));
    assert_eq!(v["outstanding_resize"]["sent_at_secs"], json!(52.5));
}

#[test]
fn test_playback_snapshot_serialises() {
    let mut snap = PlaybackSnapshot {
        bytes_in: 4096,
        bytes_out: 256,
        ping_recv_count: 2,
        pong_send_count: 2,
        writer_dropped_count: 1,
        unknown_opcode_count: 4,
        last_unknown_opcode: Some(0xC0DE),
        current_session: Some(PlaybackSessionInfo {
            started_at_secs: 3.5,
            mm_time_at_start: 0xAB_CD_EF_12,
            sample_rate_hz: 48000,
            channels: 2,
            codec: PlaybackCodec::Opus,
        }),
        start_count: 2,
        stop_count: 1,
        data_packets_received: 100,
        data_packets_decoded: 98,
        data_packets_decode_failed: 2,
        data_bytes_received: 65536,
        pcm_bytes_produced: 1_048_576,
        device_callbacks_total: 500,
        device_underrun_count: 7,
        ring_overflow_count: 3,
        samples_consumed_total: 96_000,
        last_volume_per_channel: vec![32768, 32768],
        last_mute: Some(false),
        last_latency_ms: Some(40),
        ..Default::default()
    };
    snap.recent_decode_durations_us.push_back(180);
    snap.recent_decode_durations_us.push_back(220);
    snap.messages_recv_by_opcode.insert(101, 100);
    snap.messages_recv_by_opcode.insert(103, 2);
    snap.messages_send_by_opcode.insert(3, 2);
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Transport common + baseline.
    assert_baseline_channel_fields(
        &v,
        1,
        4,
        49374,
        json!({"101": 100, "103": 2}),
        json!({"3": 2}),
    );
    assert_eq!(v["bytes_in"], json!(4096));

    // Per-session.
    let session = &v["current_session"];
    assert_eq!(session["started_at_secs"], json!(3.5));
    assert_eq!(session["mm_time_at_start"], json!(2_882_400_018u64));
    assert_eq!(session["sample_rate_hz"], json!(48000));
    assert_eq!(session["channels"], json!(2));
    // Adjacently tagged enum: a unit variant is `kind` alone.
    assert_eq!(session["codec"], json!({"kind": "opus"}));
    assert_eq!(v["start_count"], json!(2));
    assert_eq!(v["stop_count"], json!(1));

    // Data plumbing.
    assert_eq!(v["data_packets_received"], json!(100));
    assert_eq!(v["data_packets_decoded"], json!(98));
    assert_eq!(v["data_packets_decode_failed"], json!(2));
    assert_eq!(v["data_bytes_received"], json!(65536));
    assert_eq!(v["pcm_bytes_produced"], json!(1_048_576));
    assert_eq!(v["recent_decode_durations_us"], json!([180, 220]));

    // Device-side.
    assert_eq!(v["device_callbacks_total"], json!(500));
    assert_eq!(v["device_underrun_count"], json!(7));
    assert_eq!(v["ring_overflow_count"], json!(3));
    assert_eq!(v["samples_consumed_total"], json!(96000));

    // Server-controlled params.
    assert_eq!(v["last_volume_per_channel"], json!([32768, 32768]));
    assert_eq!(v["last_mute"], json!(false));
    assert_eq!(v["last_latency_ms"], json!(40));
}

#[test]
fn test_playback_codec_round_trips() {
    // Tuple-variant Other is the failure-prone case; check it
    // serialises with `kind` + `value` as expected. Whole-value
    // equality additionally pins down that the unit variants emit
    // no `value` key, which the substring checks could not see.
    fn as_value(codec: &PlaybackCodec) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(codec).unwrap()).unwrap()
    }
    assert_eq!(as_value(&PlaybackCodec::Raw), json!({"kind": "raw"}));
    assert_eq!(as_value(&PlaybackCodec::Opus), json!({"kind": "opus"}));
    assert_eq!(
        as_value(&PlaybackCodec::Other(42)),
        json!({"kind": "other", "value": 42})
    );
}

#[test]
fn test_usbredir_snapshot_serialises() {
    let mut snap = UsbredirSnapshot {
        bytes_in: 8192,
        bytes_out: 512,
        ping_recv_count: 1,
        pong_send_count: 1,
        writer_dropped_count: 0,
        unknown_opcode_count: 2,
        last_unknown_opcode: Some(0xDEAD),
        device_connect_total: 1,
        device_disconnect_total: 0,
        last_device_event_ts_secs: Some(1.5),
        server_caps: 0x0000_00ff,
        client_caps: 0x0000_001a,
        ..Default::default()
    };
    snap.messages_recv_by_opcode.insert(1, 50);
    snap.messages_recv_by_opcode.insert(2, 10);
    snap.messages_send_by_opcode.insert(1, 1);
    snap.redirected_devices.push(RedirectedDevice {
        vendor_id: 0x1d6b,
        product_id: 0x0104,
        device_class: 0x08,
        attached_at_secs: 1.5,
        bytes_to_guest: 0,
        bytes_from_guest: 0,
    });
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Transport common.
    assert_eq!(v["bytes_in"], json!(8192));
    assert_eq!(v["bytes_out"], json!(512));
    assert_eq!(v["ping_recv_count"], json!(1));

    // Baseline additions.
    assert_baseline_channel_fields(&v, 0, 2, 57005, json!({"1": 50, "2": 10}), json!({"1": 1}));

    // USB-redirection specifics.
    let devices = v["redirected_devices"]
        .as_array()
        .expect("redirected_devices must serialise as an array");
    assert_eq!(devices.len(), 1);
    let device = &devices[0];
    assert_eq!(device["vendor_id"], json!(7531));
    assert_eq!(device["product_id"], json!(260));
    assert_eq!(device["device_class"], json!(8));
    assert_eq!(device["attached_at_secs"], json!(1.5));
    assert_eq!(device["bytes_to_guest"], json!(0));
    assert_eq!(device["bytes_from_guest"], json!(0));
    assert_eq!(v["device_connect_total"], json!(1));
    assert_eq!(v["device_disconnect_total"], json!(0));
    assert_eq!(v["last_device_event_ts_secs"], json!(1.5));

    // Protocol caps.
    assert_eq!(v["server_caps"], json!(255));
    assert_eq!(v["client_caps"], json!(26));
}

#[test]
fn test_webdav_snapshot_serialises() {
    let mut snap = WebdavSnapshot {
        bytes_in: 4096,
        bytes_out: 2048,
        ping_recv_count: 2,
        pong_send_count: 2,
        writer_dropped_count: 0,
        unknown_opcode_count: 1,
        last_unknown_opcode: Some(0xBEEF),
        http_requests_received: 3,
        http_response_bytes_sent: 65536,
        active_session_count: 1,
        last_request_ts_secs: Some(0.5),
        last_response_ts_secs: Some(0.9),
        decompressed_size_limit_exceeded_count: 1,
        ..Default::default()
    };
    snap.messages_recv_by_opcode.insert(1, 10);
    snap.messages_recv_by_opcode.insert(5, 2);
    snap.messages_send_by_opcode.insert(3, 7);
    let text = serde_json::to_string_pretty(&snap).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Transport common.
    assert_eq!(v["bytes_in"], json!(4096));
    assert_eq!(v["bytes_out"], json!(2048));
    assert_eq!(v["ping_recv_count"], json!(2));

    // Baseline additions.
    assert_baseline_channel_fields(&v, 0, 1, 48879, json!({"1": 10, "5": 2}), json!({"3": 7}));

    // HTTP / WebDAV specifics.
    assert_eq!(v["http_requests_received"], json!(3));
    assert_eq!(v["http_response_bytes_sent"], json!(65536));
    assert_eq!(v["active_session_count"], json!(1));
    assert_eq!(v["last_request_ts_secs"], json!(0.5));
    assert_eq!(v["last_response_ts_secs"], json!(0.9));
    assert_eq!(v["decompressed_size_limit_exceeded_count"], json!(1));
}
#[test]
fn test_channel_snapshots_new() {
    let snapshots = ChannelSnapshots::new();
    let display = snapshots.display.lock().unwrap();
    assert_eq!(display.bytes_in, 0);
    assert_eq!(display.recent_decodes.len(), 0);
}
