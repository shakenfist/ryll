#![no_main]

use libfuzzer_sys::fuzz_target;
use shakenfist_spice_protocol::messages::vd_agent::{
    AnnounceCapabilities, Clipboard, ClipboardGrab, ClipboardRelease, ClipboardRequest,
    ClipboardWireType, MonitorsConfig, VdAgentMessageHeader, VdAgentReply,
};
use shakenfist_spice_protocol::messages::{
    AgentDisconnected, AgentTokens, BinaryData, BitmapHeader, BitmapPayload, ChannelsList, Clip,
    CursorInit, CursorInvalOne, CursorMove, CursorSet, DisplayHead, DisplayMonitorsConfig,
    DrawBase, DrawCopy, ImageDescriptor, InputsInit, KeyModifiers, MainInit, MainMouseMode,
    MultiMediaTime, Notify, Ping, Rect, SetAck, SpiceCopy, SpiceImage, SpicePoint, SpiceQMask,
    StreamActivateReport, StreamClip, StreamCreate, StreamData, StreamDataSized, StreamDestroy,
    SurfaceCreate, SurfaceDestroy, WireType,
};

// The readers ryll runs on server-to-client messages, plus the layout types
// they are built from. The first input byte selects a reader and the rest is
// the body. If the reader accepts the body, the value must survive
// write -> decode unchanged.
fn check<T: WireType + PartialEq + std::fmt::Debug>(body: &[u8]) {
    if let Ok(value) = T::decode(body) {
        let mut written = Vec::new();
        value.write(&mut written);
        let back = T::decode(&written).expect("a written value reads back");
        assert_eq!(back, value);
    }
}

fn check_clipboard<T: ClipboardWireType + PartialEq + std::fmt::Debug>(body: &[u8]) {
    for has_selection in [false, true] {
        if let Ok(value) = T::decode_with(body, has_selection) {
            let mut written = Vec::new();
            value.write_with(&mut written, has_selection);
            let back = T::decode_with(&written, has_selection).expect("a written value reads back");
            assert_eq!(back, value);
        }
    }
}

const READERS: u8 = 45;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    match selector % READERS {
        0 => check::<Ping>(body),
        1 => check::<SetAck>(body),
        2 => check::<Notify>(body),
        3 => check::<MainInit>(body),
        4 => check::<ChannelsList>(body),
        5 => check::<MainMouseMode>(body),
        6 => check::<MultiMediaTime>(body),
        7 => check::<AgentTokens>(body),
        8 => check::<AgentDisconnected>(body),
        9 => check::<VdAgentMessageHeader>(body),
        10 => check::<MonitorsConfig>(body),
        11 => check::<AnnounceCapabilities>(body),
        12 => check::<VdAgentReply>(body),
        13 => check_clipboard::<ClipboardGrab>(body),
        14 => check_clipboard::<ClipboardRequest>(body),
        15 => check_clipboard::<Clipboard>(body),
        16 => check_clipboard::<ClipboardRelease>(body),
        17 => check::<InputsInit>(body),
        18 => check::<KeyModifiers>(body),
        19 => check::<CursorInit>(body),
        20 => check::<CursorSet>(body),
        21 => check::<CursorMove>(body),
        22 => check::<CursorInvalOne>(body),
        23 => check::<Rect>(body),
        24 => check::<Clip>(body),
        25 => check::<DrawBase>(body),
        26 => check::<SpicePoint>(body),
        27 => check::<SpiceQMask>(body),
        28 => check::<ImageDescriptor>(body),
        29 => check::<BitmapHeader>(body),
        30 => check::<BitmapPayload>(body),
        31 => check::<BinaryData>(body),
        32 => check::<SpiceImage>(body),
        33 => check::<SpiceCopy>(body),
        34 => check::<DrawCopy>(body),
        35 => check::<SurfaceCreate>(body),
        36 => check::<SurfaceDestroy>(body),
        37 => check::<DisplayHead>(body),
        38 => check::<DisplayMonitorsConfig>(body),
        39 => check::<StreamCreate>(body),
        40 => check::<StreamData>(body),
        41 => check::<StreamDataSized>(body),
        42 => check::<StreamClip>(body),
        43 => check::<StreamDestroy>(body),
        44 => check::<StreamActivateReport>(body),
        _ => unreachable!("selector is reduced modulo READERS"),
    }
});
