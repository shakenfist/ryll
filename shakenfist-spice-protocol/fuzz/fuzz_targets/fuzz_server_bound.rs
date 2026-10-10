#![no_main]

use libfuzzer_sys::fuzz_target;
use shakenfist_spice_protocol::messages::vd_agent::{
    AnnounceCapabilities, Clipboard, ClipboardGrab, ClipboardRelease, ClipboardRequest,
    ClipboardWireType, MonitorsConfig, VdAgentMessageHeader, VdAgentReply,
};
use shakenfist_spice_protocol::messages::{
    AckSync, AgentTokens, Disconnecting, DisplayInit, KeyEvent, KeyModifiers, KeyScancode,
    MouseButton, MouseModeRequest, MouseMotion, MousePosition, Pong, PreferredCompression,
    PreferredVideoCodecType, StreamReport, WireType,
};

// The readers a SPICE server or proxy (kerbside, andris) runs on
// client-to-server messages. The first input byte selects a reader and the
// rest is the body. If the reader accepts the body, the value must survive
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

const READERS: u8 = 22;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    match selector % READERS {
        0 => check::<Pong>(body),
        1 => check::<AckSync>(body),
        2 => check::<Disconnecting>(body),
        3 => check::<MouseModeRequest>(body),
        4 => check::<AgentTokens>(body),
        5 => check::<VdAgentMessageHeader>(body),
        6 => check::<MonitorsConfig>(body),
        7 => check::<AnnounceCapabilities>(body),
        8 => check::<VdAgentReply>(body),
        9 => check_clipboard::<ClipboardGrab>(body),
        10 => check_clipboard::<ClipboardRequest>(body),
        11 => check_clipboard::<Clipboard>(body),
        12 => check_clipboard::<ClipboardRelease>(body),
        13 => check::<KeyEvent>(body),
        14 => check::<KeyScancode>(body),
        15 => check::<KeyModifiers>(body),
        16 => check::<MouseMotion>(body),
        17 => check::<MousePosition>(body),
        18 => check::<MouseButton>(body),
        19 => check::<DisplayInit>(body),
        20 => check::<StreamReport>(body),
        _ => {
            check::<PreferredCompression>(body);
            check::<PreferredVideoCodecType>(body);
        }
    }
});
