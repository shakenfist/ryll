// ryll web frontend — Phase 4 + 5c + 6c client.
//
// Reads the per-launch token from window.location.search,
// constructs an RTCPeerConnection, opens a "control-seed"
// data channel BEFORE generating the offer (required so the
// SDP carries an m=application section that the server
// bridge can answer with its control DC), drives the SDP
// exchange via POST /offer, and attaches the incoming video
// track to the <video> element.
//
// Phase 5c additions:
//   * KeyboardEvent.code → AT scancode table (ported from
//     `scancode_for_logical_key` in
//     shakenfist-spice-renderer/src/channels/inputs.rs).
//     Extended keys (E0-prefixed) are encoded with the
//     prefix byte in the low byte of the u32, matching
//     `make_scancode()` on the Rust side.
//   * keydown / keyup listeners on `document`, dispatched
//     through the data channel as `{type:"key",scancode,down}`.
//   * mousemove / mousedown / mouseup on the `<video>`
//     element with letterbox-corrected normalised
//     coordinates, dispatched as `{type:"pointer-move"}`
//     and `{type:"pointer-button"}`.
//   * On PC connectionState=connected, send an initial
//     `{type:"viewport",width,height}` message so the
//     guest's vdagent resizes the display via
//     VDAgentMonitorsConfig.
//
// Phase 6c additions:
//   * Browser-side auto-reconnect with exponential backoff
//     (1 s, 2 s, 4 s, 8 s, 16 s; max 5 attempts).
//   * scheduleReconnect() / resetPeerConnection() helpers.
//   * "Click to reconnect" button revealed after max attempts.
//   * connect() is now a callable function; the IIFE calls it
//     and chains .catch() → scheduleReconnect().
//   * Input listeners stay registered across reconnects — they
//     reach the module-level `dc` reference via sendCtrl().

(() => {
    'use strict';

    const statusEl = document.getElementById('status');
    const videoEl = document.getElementById('video');
    const cursorEl = document.getElementById('cursor');

    const params = new URLSearchParams(window.location.search);
    const TOKEN = params.get('token');
    if (!TOKEN) {
        statusEl.textContent = 'Missing token in URL';
        return;
    }

    const setStatus = (msg) => {
        statusEl.textContent = msg;
        console.log('[ryll]', msg);
    };

    // ---------------------------------------------------------------
    // Reconnect state.
    // ---------------------------------------------------------------
    const RECONNECT_BACKOFFS_MS = [1000, 2000, 4000, 8000, 16000];
    let reconnectAttempt = 0;
    // Set when the user clicks the Disconnect button, so the
    // ICE/PC state-change handlers don't reopen the connection
    // we just intentionally tore down.
    let manuallyDisconnected = false;

    // Module-level pc / dc references updated by connect().
    let pc = null;
    let dc = null;

    // ---------------------------------------------------------------
    // KeyboardEvent.code → AT scancode wire value (the PRESS form).
    //
    // Ported from `scancode_for_logical_key` in
    // shakenfist-spice-renderer/src/channels/inputs.rs. Each value
    // is the spice-gtk-compatible wire u32 that goes straight into
    // the SPICE KEY_DOWN message's `code` field. The SPICE server
    // iterates the bytes of this u32 starting at the low byte and
    // feeds them to the guest's PS/2 controller, so the BYTE ORDER
    // matters:
    //
    //   * Non-extended keys: single-byte scancode in the low byte
    //     (e.g. 'a' = 0x1E).
    //   * Extended keys (E0-prefixed on the AT keyboard, i.e. arrows,
    //     navigation cluster, right-side modifiers): two bytes —
    //     0xE0 prefix in the LOW byte, scancode in the next byte.
    //     This matches `make_scancode()` on the Rust side:
    //       wire = (scancode << 8) | 0xE0
    //     i.e. Up arrow's base 0x48 becomes 0x48E0 on the wire.
    //     (Bytes: E0 48 00 00 little-endian.)
    //
    // The KEY_UP form sets bit 0x80 on the byte that carries the
    // scancode — see `releaseScancode` below for the transform.
    //
    // Modifier keys aren't in the LogicalKey table (the GUI
    // path lets egui's modifier state ride along on the
    // VKEY events) but the SPICE inputs channel happily
    // accepts them as standalone scancodes. The mapping below
    // matches spice-gtk's host_to_pc_scancode() for ShiftLeft,
    // ShiftRight, ControlLeft, ControlRight, AltLeft, AltRight.
    // ---------------------------------------------------------------
    const SCANCODE_TABLE = {
        // Letters (A..Z)
        KeyA: 0x1E, KeyB: 0x30, KeyC: 0x2E, KeyD: 0x20, KeyE: 0x12,
        KeyF: 0x21, KeyG: 0x22, KeyH: 0x23, KeyI: 0x17, KeyJ: 0x24,
        KeyK: 0x25, KeyL: 0x26, KeyM: 0x32, KeyN: 0x31, KeyO: 0x18,
        KeyP: 0x19, KeyQ: 0x10, KeyR: 0x13, KeyS: 0x1F, KeyT: 0x14,
        KeyU: 0x16, KeyV: 0x2F, KeyW: 0x11, KeyX: 0x2D, KeyY: 0x15,
        KeyZ: 0x2C,

        // Digits 1..9, 0
        Digit1: 0x02, Digit2: 0x03, Digit3: 0x04, Digit4: 0x05, Digit5: 0x06,
        Digit6: 0x07, Digit7: 0x08, Digit8: 0x09, Digit9: 0x0A, Digit0: 0x0B,

        // Function keys
        F1: 0x3B, F2: 0x3C, F3: 0x3D, F4: 0x3E, F5: 0x3F,
        F6: 0x40, F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44,
        F11: 0x57, F12: 0x58,

        // Whitespace / control cluster
        Space: 0x39,
        Enter: 0x1C,
        Tab: 0x0F,
        Backspace: 0x0E,
        Escape: 0x01,
        CapsLock: 0x3A,

        // Punctuation
        Backquote: 0x29,
        Minus: 0x0C,
        Equal: 0x0D,
        BracketLeft: 0x1A,
        BracketRight: 0x1B,
        Backslash: 0x2B,
        Semicolon: 0x27,
        Quote: 0x28,
        Comma: 0x33,
        Period: 0x34,
        Slash: 0x35,
        IntlBackslash: 0x56,

        // Modifiers — left-side keys are non-extended, right-side
        // keys are E0-prefixed extended scancodes (low-byte 0xE0,
        // next byte is the scancode).
        ShiftLeft: 0x2A,
        ShiftRight: 0x36,
        ControlLeft: 0x1D,
        ControlRight: 0x1DE0,
        AltLeft: 0x38,
        AltRight: 0x38E0,
        MetaLeft: 0x5BE0,
        MetaRight: 0x5CE0,
        ContextMenu: 0x5DE0,

        // Navigation cluster (all E0-prefixed)
        Insert: 0x52E0,
        Delete: 0x53E0,
        Home: 0x47E0,
        End: 0x4FE0,
        PageUp: 0x49E0,
        PageDown: 0x51E0,

        // Arrow keys (all E0-prefixed)
        ArrowUp: 0x48E0,
        ArrowDown: 0x50E0,
        ArrowLeft: 0x4BE0,
        ArrowRight: 0x4DE0,

        // Numpad — covers the common subset; the SPICE side
        // accepts these as either NumLock'd or unshifted
        // depending on the modifier state, which the guest
        // resolves itself.
        Numpad0: 0x52, Numpad1: 0x4F, Numpad2: 0x50, Numpad3: 0x51,
        Numpad4: 0x4B, Numpad5: 0x4C, Numpad6: 0x4D, Numpad7: 0x47,
        Numpad8: 0x48, Numpad9: 0x49,
        NumpadDecimal: 0x53,
        NumpadAdd: 0x4E,
        NumpadSubtract: 0x4A,
        NumpadMultiply: 0x37,
        NumpadDivide: 0x35E0,
        NumpadEnter: 0x1CE0,
        NumLock: 0x45,
        ScrollLock: 0x46,
        Pause: 0x45,  // approximate; full Pause is multi-byte
        PrintScreen: 0x37E0,
    };

    // Derive the KEY_UP wire scancode from the KEY_DOWN value. The
    // AT keyboard's break code is the make code with bit 0x80 set
    // on the byte that carries the scancode; the SPICE server
    // passes the bytes through to the guest's PS/2 controller
    // unchanged, so KEY_UP MUST carry the release-form scancode or
    // the guest sees two consecutive make codes and either floods
    // the input layer with auto-repeats or treats the second press
    // as a no-op (Linux atkbd handles consecutive makes
    // inconsistently). Matches `make_scancode(base, true)` in
    // shakenfist-spice-renderer/src/channels/inputs.rs:
    //   * Non-extended:           sc | 0x80
    //   * Extended (E0 in byte 0): sc | 0x8000  (high bit on byte 1)
    const releaseScancode = (press) => {
        if (press < 0x100) {
            return press | 0x80;
        }
        // Extended: byte 0 is 0xE0, byte 1 is the scancode. Set
        // bit 0x80 on byte 1.
        return press | 0x8000;
    };

    // Self-check the scancode transforms at startup so a regression
    // in the table or the release helper fails loudly in the
    // browser console rather than silently flooding the guest with
    // bad keystrokes. The expected values come from running
    // `make_scancode(base, release)` in
    // shakenfist-spice-renderer/src/channels/inputs.rs by hand for
    // a few canonical keys:
    //   * KeyA       (base 0x1E):  press 0x1E,   release 0x9E
    //   * ArrowUp    (base 0x148): press 0x48E0, release 0xC8E0
    //   * NumpadEnter(base 0x11C): press 0x1CE0, release 0x9CE0
    (() => {
        const cases = [
            ['KeyA', 0x1E, 0x9E],
            ['ArrowUp', 0x48E0, 0xC8E0],
            ['NumpadEnter', 0x1CE0, 0x9CE0],
        ];
        for (const [code, expectPress, expectRelease] of cases) {
            const press = SCANCODE_TABLE[code];
            const release = releaseScancode(press);
            if (press !== expectPress || release !== expectRelease) {
                console.error(
                    `[ryll] scancode self-check FAILED for ${code}: ` +
                    `press=0x${press.toString(16)} (want 0x${expectPress.toString(16)}), ` +
                    `release=0x${release.toString(16)} (want 0x${expectRelease.toString(16)})`,
                );
            }
        }
    })();

    // SPICE button bitmask values (from
    // shakenfist-spice-protocol/src/constants.rs::mouse_buttons).
    // Browser MouseEvent.button: 0=primary, 1=middle, 2=secondary.
    const SPICE_BUTTON_LEFT = 1;
    const SPICE_BUTTON_MIDDLE = 2;
    const SPICE_BUTTON_RIGHT = 4;
    const browserButtonToSpice = (button) => {
        switch (button) {
            case 0: return SPICE_BUTTON_LEFT;
            case 1: return SPICE_BUTTON_MIDDLE;
            case 2: return SPICE_BUTTON_RIGHT;
            default: return 0;
        }
    };

    // ---------------------------------------------------------------
    // Pointer coordinate normalisation with letterbox correction.
    //
    // The <video> element's bounding rect may be larger than the
    // actually-rendered video area when the source aspect ratio
    // doesn't match the element's. Compute the rendered area
    // from videoWidth / videoHeight and offset the pointer into
    // it before normalising to [0, 1].
    // ---------------------------------------------------------------
    const pointerToNorm = (e) => {
        const rect = videoEl.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) {
            return null;
        }
        const elemAspect = rect.width / rect.height;
        const vw = videoEl.videoWidth;
        const vh = videoEl.videoHeight;
        const videoAspect = vw > 0 && vh > 0 ? vw / vh : NaN;
        if (!isFinite(videoAspect) || videoAspect <= 0) {
            // No video metadata yet — just normalise over the
            // element rect. The Rust side will denormalise
            // against whatever primary surface is present.
            const x = (e.clientX - rect.left) / rect.width;
            const y = (e.clientY - rect.top) / rect.height;
            return { x_norm: clamp01(x), y_norm: clamp01(y) };
        }
        if (elemAspect > videoAspect) {
            // Pillarboxed: black bars on left/right.
            const renderedWidth = rect.height * videoAspect;
            const padX = (rect.width - renderedWidth) / 2;
            const x = (e.clientX - rect.left - padX) / renderedWidth;
            const y = (e.clientY - rect.top) / rect.height;
            return { x_norm: clamp01(x), y_norm: clamp01(y) };
        } else {
            // Letterboxed: black bars on top/bottom.
            const renderedHeight = rect.width / videoAspect;
            const padY = (rect.height - renderedHeight) / 2;
            const x = (e.clientX - rect.left) / rect.width;
            const y = (e.clientY - rect.top - padY) / renderedHeight;
            return { x_norm: clamp01(x), y_norm: clamp01(y) };
        }
    };

    const clamp01 = (v) => Math.max(0, Math.min(1, v));

    // ---------------------------------------------------------------
    // sendCtrl uses the module-level `dc` reference so that
    // input listeners registered once continue to work across
    // reconnects without re-registration.
    // ---------------------------------------------------------------
    const sendCtrl = (obj) => {
        if (!dc || dc.readyState !== 'open') {
            return;
        }
        try {
            dc.send(JSON.stringify(obj));
        } catch (err) {
            console.warn('[ryll] dc.send failed:', err);
        }
    };

    // ---------------------------------------------------------------
    // Keyboard listeners — bound on `document` because the
    // <video> element doesn't naturally have keyboard focus.
    // We preventDefault on every recognised key so browser
    // shortcuts (Ctrl+W, Ctrl+T, etc.) don't intercept the
    // input destined for the guest. F11 is allowed through so
    // browser fullscreen still works.
    //
    // Browser-generated OS auto-repeat (`KeyboardEvent.repeat`)
    // is intentionally dropped: the guest's input layer
    // already does its own auto-repeat with its own initial
    // delay + rate, and forwarding the host's repeat stream as
    // a flood of make-codes (without intervening break-codes)
    // makes the guest interpret keys unpredictably — either
    // lost, or massively duplicated. Match the GUI frontend's
    // behaviour (`ryll/src/app.rs:2592` filters egui events
    // with `repeat: false`) so the wire carries exactly one
    // press + one release per physical keystroke.
    // ---------------------------------------------------------------
    const KEY_PASSTHROUGH = new Set(['F11']);

    document.addEventListener('keydown', (e) => {
        if (e.repeat) {
            return;
        }
        const sc = SCANCODE_TABLE[e.code];
        if (sc === undefined) {
            return;
        }
        if (!KEY_PASSTHROUGH.has(e.code)) {
            e.preventDefault();
        }
        sendCtrl({ type: 'key', scancode: sc, down: true });
    });

    document.addEventListener('keyup', (e) => {
        const sc = SCANCODE_TABLE[e.code];
        if (sc === undefined) {
            return;
        }
        if (!KEY_PASSTHROUGH.has(e.code)) {
            e.preventDefault();
        }
        sendCtrl({ type: 'key', scancode: releaseScancode(sc), down: false });
    });

    // ---------------------------------------------------------------
    // Pointer listeners on <video>. Suppress the default context
    // menu so right-clicks reach the guest.
    // ---------------------------------------------------------------
    videoEl.addEventListener('contextmenu', (e) => e.preventDefault());

    videoEl.addEventListener('mousemove', (e) => {
        const norm = pointerToNorm(e);
        if (!norm) return;
        sendCtrl({ type: 'pointer-move', x_norm: norm.x_norm, y_norm: norm.y_norm });
    });

    videoEl.addEventListener('mousedown', (e) => {
        const norm = pointerToNorm(e);
        if (!norm) return;
        const button = browserButtonToSpice(e.button);
        if (!button) return;
        e.preventDefault();
        sendCtrl({
            type: 'pointer-button',
            button,
            down: true,
            x_norm: norm.x_norm,
            y_norm: norm.y_norm,
        });
    });

    videoEl.addEventListener('mouseup', (e) => {
        const norm = pointerToNorm(e);
        if (!norm) return;
        const button = browserButtonToSpice(e.button);
        if (!button) return;
        e.preventDefault();
        sendCtrl({
            type: 'pointer-button',
            button,
            down: false,
            x_norm: norm.x_norm,
            y_norm: norm.y_norm,
        });
    });

    // ---------------------------------------------------------------
    // Audio-enable button wired once (stays across reconnects).
    // ---------------------------------------------------------------
    const enableAudioBtn = document.getElementById('enable-audio');
    enableAudioBtn.addEventListener('click', () => {
        videoEl.muted = false;
        enableAudioBtn.hidden = true;
        // Re-trigger play in case the browser paused on un-mute.
        videoEl.play().catch(err => console.warn('[ryll] play after unmute failed:', err));
    });

    // ---------------------------------------------------------------
    // Disconnect button — revealed on successful connection. Closes
    // the peer connection and suppresses auto-reconnect so test
    // harnesses (e.g. ./bin/runtest.sh) can drive a clean session
    // teardown from the browser side.
    // ---------------------------------------------------------------
    const disconnectBtn = document.getElementById('disconnect-btn');
    disconnectBtn.addEventListener('click', () => {
        manuallyDisconnected = true;
        disconnectBtn.hidden = true;
        enableAudioBtn.hidden = true;
        setStatus('Disconnected');
        resetPeerConnection();
        showReconnectButton();
    });

    // ---------------------------------------------------------------
    // Manual reconnect button — revealed when max backoff attempts
    // are exhausted, or after the user clicks Disconnect.
    // ---------------------------------------------------------------
    const reconnectBtn = document.getElementById('reconnect-btn');
    const showReconnectButton = () => {
        reconnectBtn.style.display = '';
    };

    reconnectBtn.addEventListener('click', () => {
        reconnectBtn.style.display = 'none';
        reconnectAttempt = 0;
        manuallyDisconnected = false;
        scheduleReconnect();
    });

    // ---------------------------------------------------------------
    // scheduleReconnect — exponential backoff, up to 5 attempts.
    // After max attempts the manual reconnect button is revealed.
    // ---------------------------------------------------------------
    function scheduleReconnect() {
        if (manuallyDisconnected) {
            return;
        }
        if (reconnectAttempt >= RECONNECT_BACKOFFS_MS.length) {
            setStatus('Disconnected. Click to reconnect.');
            showReconnectButton();
            return;
        }
        const delay = RECONNECT_BACKOFFS_MS[reconnectAttempt++];
        setStatus(`Reconnecting in ${delay / 1000}s (attempt ${reconnectAttempt})…`);
        setTimeout(() => {
            resetPeerConnection();
            connect().catch(err => {
                console.warn('[ryll] reconnect attempt failed:', err);
                scheduleReconnect();
            });
        }, delay);
    }

    // ---------------------------------------------------------------
    // resetPeerConnection — close existing PC (if any) and null it
    // so the next connect() builds a fresh RTCPeerConnection.
    // ---------------------------------------------------------------
    function resetPeerConnection() {
        if (pc) {
            try { pc.close(); } catch (e) { /* ignore */ }
            pc = null;
        }
        dc = null;
    }

    // ---------------------------------------------------------------
    // Server → browser cursor overlay state. The server forwards
    // CursorShape (PNG, base64'd) and CursorPosition events from
    // the SPICE cursor channel over the same control DC the
    // browser uses for inputs. The browser places an <img>
    // overlay above the <video> at the denormalised position
    // (letterbox-aware) and hides the host cursor over the video
    // (in style.css) so the SPICE cursor wins.
    // ---------------------------------------------------------------
    let cursorHotX = 0;
    let cursorHotY = 0;
    let cursorLastNorm = null;

    const positionCursor = (xNorm, yNorm) => {
        const rect = videoEl.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) {
            return;
        }
        // Mirror the input-side letterbox correction in
        // pointerToNorm so the overlay tracks the inputs we sent
        // exactly: compute the actually-rendered video area, then
        // place the cursor inside it at the normalised position.
        const elemAspect = rect.width / rect.height;
        const vw = videoEl.videoWidth;
        const vh = videoEl.videoHeight;
        const videoAspect = vw > 0 && vh > 0 ? vw / vh : NaN;
        let renderedX = rect.left;
        let renderedY = rect.top;
        let renderedW = rect.width;
        let renderedH = rect.height;
        if (isFinite(videoAspect) && videoAspect > 0) {
            if (elemAspect > videoAspect) {
                renderedW = rect.height * videoAspect;
                renderedX = rect.left + (rect.width - renderedW) / 2;
            } else {
                renderedH = rect.width / videoAspect;
                renderedY = rect.top + (rect.height - renderedH) / 2;
            }
        }
        cursorEl.style.left = `${renderedX + xNorm * renderedW - cursorHotX}px`;
        cursorEl.style.top = `${renderedY + yNorm * renderedH - cursorHotY}px`;
    };

    // The host browser cursor is hidden over the video only
    // when the SPICE overlay is actually showing a sprite —
    // some guest stacks (virtio-gpu + Wayland GDM) never push
    // a CursorShape over the SPICE cursor channel because they
    // render the cursor into the video stream itself, and
    // hiding the host cursor blindly would leave the user with
    // nothing to point with. Toggle `videoEl.style.cursor`
    // alongside the overlay's visibility so the host cursor
    // takes over whenever the SPICE overlay is not in play.
    const hideHostCursor = () => { videoEl.style.cursor = 'none'; };
    const showHostCursor = () => { videoEl.style.cursor = ''; };

    const handleControlMessage = (msg) => {
        switch (msg && msg.type) {
            case 'cursor-shape':
                cursorEl.src = `data:image/png;base64,${msg.png_b64}`;
                cursorHotX = msg.hot_x ?? 0;
                cursorHotY = msg.hot_y ?? 0;
                cursorEl.hidden = false;
                hideHostCursor();
                if (cursorLastNorm) {
                    positionCursor(cursorLastNorm.x, cursorLastNorm.y);
                }
                break;
            case 'cursor-pos':
                cursorLastNorm = { x: msg.x_norm, y: msg.y_norm };
                positionCursor(msg.x_norm, msg.y_norm);
                break;
            case 'cursor-hide':
                cursorEl.hidden = true;
                showHostCursor();
                break;
            case 'cursor-show':
                if (cursorEl.src) {
                    cursorEl.hidden = false;
                    hideHostCursor();
                }
                break;
            default:
                console.log('[ryll] dc message:', msg);
                break;
        }
    };

    // ---------------------------------------------------------------
    // connect() — build a new RTCPeerConnection, wire all PC/DC
    // callbacks, and drive the offer/answer SDP exchange.
    // Re-callable on each reconnect attempt.
    // ---------------------------------------------------------------
    async function connect() {
        setStatus('Negotiating…');

        // Build a brand-new PC each time so we never reuse a failed
        // connection object (some browsers cache failed PCs briefly).
        pc = new RTCPeerConnection();

        // Phase 3 finding: a data channel must exist on the offer
        // side before createOffer() so the SDP carries an
        // m=application section. The server bridge's control DC is
        // answered against this seed channel.
        dc = pc.createDataChannel('control-seed', { ordered: true });

        dc.onopen = () => {
            console.log('[ryll] data channel open');
        };
        dc.onclose = () => {
            console.log('[ryll] data channel closed');
        };
        dc.onmessage = (event) => {
            let msg;
            try {
                const text = typeof event.data === 'string'
                    ? event.data
                    : new TextDecoder().decode(event.data);
                msg = JSON.parse(text);
            } catch (err) {
                console.warn('[ryll] invalid control message:', err);
                return;
            }
            handleControlMessage(msg);
        };

        // Receive the server's video and audio tracks.
        pc.ontrack = (event) => {
            console.log('[ryll] ontrack kind=', event.track.kind);
            if (event.track.kind === 'video' && event.streams[0]) {
                videoEl.srcObject = event.streams[0];
                setStatus('Connected');
                // Reveal the audio-toggle and disconnect buttons now
                // that we have a stream.
                enableAudioBtn.hidden = false;
                disconnectBtn.hidden = false;
            }
            // Audio plays via the browser's default sink; the
            // <video> element with the same MediaStream object
            // handles audio rendering implicitly.
        };

        pc.oniceconnectionstatechange = () => {
            console.log('[ryll] ICE state:', pc.iceConnectionState);
            if (pc.iceConnectionState === 'failed' ||
                    pc.iceConnectionState === 'disconnected') {
                scheduleReconnect();
            }
        };

        // ---------------------------------------------------------------
        // Send the initial viewport message exactly once when the PC
        // reaches the connected state. Use the <video> element's
        // bounding rect as the requested resolution — the guest's
        // vdagent will resize its X session to match (via
        // VDAgentMonitorsConfig dispatched on the Rust side from
        // the resize_tx channel that this message lands on).
        // viewportSent is scoped to this connect() call so it
        // retriggers correctly on reconnect.
        // ---------------------------------------------------------------
        let viewportSent = false;
        const sendViewport = () => {
            if (viewportSent) return;
            const rect = videoEl.getBoundingClientRect();
            const w = Math.round(rect.width);
            const h = Math.round(rect.height);
            if (w <= 0 || h <= 0) return;
            viewportSent = true;
            sendCtrl({ type: 'viewport', width: w, height: h });
            console.log('[ryll] viewport sent:', w, 'x', h);
        };

        pc.onconnectionstatechange = () => {
            console.log('[ryll] PC state:', pc.connectionState);
            if (pc.connectionState === 'connected') {
                // Reset backoff counter on successful connection.
                reconnectAttempt = 0;
                sendViewport();
            } else if (pc.connectionState === 'failed') {
                scheduleReconnect();
            }
        };

        // Tell the server we're a recvonly viewer for both video
        // and audio.
        pc.addTransceiver('video', { direction: 'recvonly' });
        pc.addTransceiver('audio', { direction: 'recvonly' });

        const waitForIceComplete = () => new Promise((resolve) => {
            if (pc.iceGatheringState === 'complete') {
                resolve();
                return;
            }
            const onChange = () => {
                if (pc.iceGatheringState === 'complete') {
                    pc.removeEventListener('icegatheringstatechange', onChange);
                    resolve();
                }
            };
            pc.addEventListener('icegatheringstatechange', onChange);
        });

        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        await waitForIceComplete();

        const finalOffer = pc.localDescription;
        const response = await fetch(`/offer?token=${encodeURIComponent(TOKEN)}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                type: finalOffer.type,
                sdp: finalOffer.sdp,
            }),
        });
        if (!response.ok) {
            setStatus(`Server: ${response.status} ${response.statusText}`);
            throw new Error(`offer rejected: ${response.status}`);
        }
        const answer = await response.json();
        await pc.setRemoteDescription(new RTCSessionDescription(answer));
    }

    // Initial connection — failures feed into the reconnect schedule.
    connect().catch(err => {
        console.error('[ryll] initial connect failed:', err);
        scheduleReconnect();
    });
})();
