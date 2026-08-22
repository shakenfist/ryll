#!/usr/bin/env python3
"""Ask a browser what it would really offer for WebRTC video.

`docs/development.md` warns that `RTCRtpReceiver.getCapabilities()` is
not evidence: Firefox lists H.264 there while omitting it from the
offer it actually sends, which is the whole of issue #289. This is the
check that warning asks for, made runnable.

It serves a one-page site, points a browser at it, and prints the
codecs from the offer that page generated -- with `getCapabilities`
beside them, so the discrepancy is visible rather than asserted.

    tools/browser-offer-probe.py                       # default browser
    tools/browser-offer-probe.py --browser chromium
    tools/browser-offer-probe.py --browser firefox-esr --sdp

ryll sends video and does not receive it, so the transceiver defaults
to `recvonly` -- the direction that decides whether a viewer sees a
picture. `--direction sendrecv` asks the other question.

A browser is launched with a throwaway profile by default, which is
usually *not* what you want for Firefox: a fresh profile has never
downloaded the OpenH264 plugin, so it would report no H.264 for a
reason that has nothing to do with the browser you meant to test.
Pass `--profile` with a real profile directory to test a provisioned
browser.
"""

import argparse
import http.server
import json
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading

PAGE = """<!doctype html><meta charset=utf-8><title>ryll offer probe</title>
<body style="font-family: system-ui; padding: 2rem">
<p>Generating a WebRTC offer&hellip; this page closes itself.</p>
<script>
(async () => {
  const out = { ua: navigator.userAgent };
  const names = (list) => list.codecs.map(c =>
      c.mimeType + (c.sdpFmtpLine ? ' ' + c.sdpFmtpLine : ''));
  try { out.send_caps = names(RTCRtpSender.getCapabilities('video')); }
  catch (e) { out.send_caps = ['error: ' + e]; }
  try { out.recv_caps = names(RTCRtpReceiver.getCapabilities('video')); }
  catch (e) { out.recv_caps = ['error: ' + e]; }
  try {
    const pc = new RTCPeerConnection();
    pc.addTransceiver('video', { direction: 'DIRECTION' });
    pc.addTransceiver('audio', { direction: 'DIRECTION' });
    out.sdp = (await pc.createOffer()).sdp;
  } catch (e) { out.sdp = 'error: ' + e; }
  await fetch('/result', { method: 'POST', body: JSON.stringify(out) });
})();
</script>
"""


def serve_once(port, direction, timeout):
    """Serve the probe page until the browser posts its result back."""
    body = PAGE.replace('DIRECTION', direction).encode('utf-8')
    got = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            n = int(self.headers.get('Content-Length', 0))
            got.append(json.loads(self.rfile.read(n).decode('utf-8', 'replace')))
            self.send_response(204)
            self.end_headers()
            threading.Thread(target=self.server.shutdown, daemon=True).start()

        def log_message(self, *args):
            pass

    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(('127.0.0.1', port), Handler) as srv:
        thread = threading.Thread(target=srv.serve_forever, daemon=True)
        thread.start()
        thread.join(timeout=timeout)
    return got[0] if got else None


def rtpmap_lines(sdp):
    """The `a=rtpmap` entries of the offer's first video section."""
    out, in_video = [], False
    for line in sdp.splitlines():
        if line.startswith('m='):
            in_video = line.startswith('m=video')
        elif in_video and line.startswith('a=rtpmap:'):
            out.append(line[len('a=rtpmap:'):])
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--browser', default='firefox-esr',
                    help='browser command to launch (default: firefox-esr)')
    ap.add_argument('--profile',
                    help='browser profile directory; without it a throwaway '
                         'profile is used, which for Firefox means no OpenH264')
    ap.add_argument('--direction', default='recvonly',
                    choices=['recvonly', 'sendrecv', 'sendonly'],
                    help='transceiver direction to offer (default: recvonly, '
                         'which is what a ryll viewer uses)')
    ap.add_argument('--port', type=int, default=8931, help='local port to serve on')
    ap.add_argument('--timeout', type=int, default=60, help='seconds to wait')
    ap.add_argument('--sdp', action='store_true', help='also print the whole offer SDP')
    args = ap.parse_args()

    if not shutil.which(args.browser):
        print('no such browser: %s' % args.browser, file=sys.stderr)
        return 2

    url = 'http://127.0.0.1:%d/' % args.port
    tmp_profile = None
    if args.profile:
        profile = args.profile
    else:
        tmp_profile = tempfile.mkdtemp(prefix='ryll-offer-probe-')
        profile = tmp_profile

    if 'firefox' in args.browser:
        cmd = [args.browser, '--headless', '--profile', profile, '--new-instance', url]
    else:
        cmd = [args.browser, '--headless=new', '--user-data-dir=' + profile,
               '--autoplay-policy=no-user-gesture-required', url]

    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        result = serve_once(args.port, args.direction, args.timeout)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        if tmp_profile:
            shutil.rmtree(tmp_profile, ignore_errors=True)

    if result is None:
        print('the browser never posted a result back', file=sys.stderr)
        return 1

    print(result['ua'])
    print()
    print('getCapabilities(video), sender side:')
    for entry in result['send_caps']:
        print('   ', entry)
    print()
    print('The offer it actually sent (direction=%s):' % args.direction)
    lines = rtpmap_lines(result['sdp'])
    for entry in lines:
        print('   ', entry)
    if args.sdp:
        print()
        print(result['sdp'])

    offered_h264 = any('H264' in entry.upper() for entry in lines)
    claimed_h264 = any('H264' in entry.upper() for entry in result['send_caps'])
    print()
    if offered_h264:
        print('VERDICT: offers H.264 -- ryll can send video to this browser.')
        return 0
    if claimed_h264:
        print('VERDICT: claims H.264 in getCapabilities and offers none. ryll')
        print('         encodes H.264 only, so this browser gets no video (#289).')
    else:
        print('VERDICT: no H.264 anywhere. ryll encodes H.264 only, so this')
        print('         browser gets no video (#289).')
    return 1


if __name__ == '__main__':
    sys.exit(main())
