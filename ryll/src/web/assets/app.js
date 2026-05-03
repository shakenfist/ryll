// ryll web frontend — Phase 4 client.
//
// Reads the per-launch token from window.location.search,
// constructs an RTCPeerConnection, opens a "control-seed"
// data channel BEFORE generating the offer (required so the
// SDP carries an m=application section that the server
// bridge can answer with its control DC), drives the SDP
// exchange via POST /offer, and attaches the incoming video
// track to the <video> element.
//
// Phase 5 will replace the no-op data-channel handlers with
// real input event marshalling and cursor overlay updates.

(() => {
    const statusEl = document.getElementById("status");
    const videoEl = document.getElementById("video");

    const params = new URLSearchParams(window.location.search);
    const TOKEN = params.get("token");
    if (!TOKEN) {
        statusEl.textContent = "Missing token in URL";
        return;
    }

    const setStatus = (msg) => {
        statusEl.textContent = msg;
        console.log("[ryll]", msg);
    };

    setStatus("Negotiating…");

    const pc = new RTCPeerConnection();

    // Phase 3 step 3f finding: a data channel must exist on the
    // offer side before createOffer() so the SDP carries an
    // m=application section. The server bridge's control DC
    // is answered against this seed channel; Phase 5 will use
    // it for input events and cursor overlay.
    const dc = pc.createDataChannel("control-seed", { ordered: true });
    dc.onopen = () => console.log("[ryll] data channel open");
    dc.onmessage = (e) => console.log("[ryll] dc message:", e.data);

    // Receive the server's video and audio tracks.
    pc.ontrack = (event) => {
        console.log("[ryll] ontrack kind=", event.track.kind);
        if (event.track.kind === "video" && event.streams[0]) {
            videoEl.srcObject = event.streams[0];
            setStatus("Connected");
        }
        // Audio plays via the browser's default sink; the
        // <video> element with the same MediaStream object
        // handles audio rendering implicitly.
    };

    pc.oniceconnectionstatechange = () => {
        console.log("[ryll] ICE state:", pc.iceConnectionState);
        if (pc.iceConnectionState === "failed") {
            setStatus("ICE failed — check the network");
        } else if (pc.iceConnectionState === "disconnected") {
            setStatus("Disconnected");
        }
    };

    // Tell the server we're a recvonly viewer for both video
    // and audio.
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });

    const waitForIceComplete = () => new Promise((resolve) => {
        if (pc.iceGatheringState === "complete") {
            resolve();
            return;
        }
        const onChange = () => {
            if (pc.iceGatheringState === "complete") {
                pc.removeEventListener("icegatheringstatechange", onChange);
                resolve();
            }
        };
        pc.addEventListener("icegatheringstatechange", onChange);
    });

    const connect = async () => {
        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        await waitForIceComplete();

        const finalOffer = pc.localDescription;
        const response = await fetch(`/offer?token=${encodeURIComponent(TOKEN)}`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
                type: finalOffer.type,
                sdp: finalOffer.sdp,
            }),
        });
        if (!response.ok) {
            setStatus(`Server: ${response.status} ${response.statusText}`);
            return;
        }
        const answer = await response.json();
        await pc.setRemoteDescription(new RTCSessionDescription(answer));
    };

    connect().catch((err) => {
        console.error("[ryll] connect failed:", err);
        setStatus(`Error: ${err.message ?? err}`);
    });
})();
