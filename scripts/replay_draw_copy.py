#!/usr/bin/env python3
import argparse
import json
import re
from pathlib import Path


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


LINE_RE = re.compile(
    r"(?P<ts>\d{4}-\d{2}-\d{2}T[^\s]+)?.*"
    r"surface=(?P<surface>\d+),\s*"
    r"pos=\((?P<x>\d+),(?P<y>\d+)\),\s*"
    r"size=(?P<w>\d+)x(?P<h>\d+),\s*"
    r"type=Some\((?P<typ>[^)]+)\),\s*"
    r"id=(?P<id>\d+),\s*flags=(?P<flags>\d+),\s*data_bytes=(?P<data_bytes>\d+)"
)


def parse_events(log_path: Path):
    events = []
    min_x = min_y = 10**9
    max_x = max_y = 0

    with log_path.open("r", encoding="utf-8", errors="ignore") as f:
        for line in f:
            line = ANSI_RE.sub("", line)
            m = LINE_RE.search(line)
            if not m:
                continue

            x = int(m.group("x"))
            y = int(m.group("y"))
            w = int(m.group("w"))
            h = int(m.group("h"))
            evt = {
                "ts": m.group("ts") or "n/a",
                "surface": int(m.group("surface")),
                "x": x,
                "y": y,
                "w": w,
                "h": h,
                "type": m.group("typ"),
                "id": m.group("id"),
                "data_bytes": int(m.group("data_bytes")),
            }
            events.append(evt)

            min_x = min(min_x, x)
            min_y = min(min_y, y)
            max_x = max(max_x, x + w)
            max_y = max(max_y, y + h)

    if not events:
        raise SystemExit("No draw_copy lines found in log.")

    bounds = {
        "min_x": 0 if min_x == 10**9 else min_x,
        "min_y": 0 if min_y == 10**9 else min_y,
        "width": max(1, max_x - (0 if min_x == 10**9 else min_x)),
        "height": max(1, max_y - (0 if min_y == 10**9 else min_y)),
    }
    return events, bounds


HTML_TMPL = """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>draw_copy replay</title>
  <style>
    body { font-family: system-ui, sans-serif; margin: 16px; }
    .row { display: flex; gap: 16px; align-items: flex-start; }
    canvas { border: 1px solid #333; background: #f8fafc; }
    .panel { min-width: 320px; }
    .mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12px; white-space: pre-wrap; }
    button, input { margin-right: 8px; }
  </style>
</head>
<body>
  <h3>SPICE draw_copy replay</h3>
  <div>Events: <b id="count">__EVENT_COUNT__</b> | Surface bounds: <b id="bounds">__BOUNDS_TEXT__</b> | Progress: <b id="progress">0</b></div>
  <div id="status" style="margin:6px 0;color:#b00020;font-weight:600;"></div>
  <div style="margin: 8px 0;">
    <button id="play" type="button">Play</button>
    <button id="pause" type="button">Pause</button>
    <button id="step" type="button">Step</button>
    <button id="clear" type="button">Clear</button>
    Speed: <input id="speed" type="range" min="1" max="120" value="30" />
    <span id="speedv">30</span> ev/s
  </div>
  <div class="row">
    <canvas id="cv" width="960" height="600"></canvas>
    <div class="panel">
      <div class="mono" id="meta"></div>
      <hr/>
      <div class="mono" id="evt"></div>
    </div>
  </div>
<script>
const EVENTS = __EVENTS__;
const BOUNDS = __BOUNDS__;

const cv = document.getElementById('cv');
const ctx = cv.getContext('2d');
const evtBox = document.getElementById('evt');
const meta = document.getElementById('meta');
const count = document.getElementById('count');
const boundsEl = document.getElementById('bounds');
const progress = document.getElementById('progress');
const statusEl = document.getElementById('status');
const speed = document.getElementById('speed');
const speedv = document.getElementById('speedv');

window.onerror = (msg, src, line, col) => {
  statusEl.textContent = `JS error: ${msg} @ ${line}:${col}`;
};

try {
  count.textContent = EVENTS.length;
  boundsEl.textContent = `${BOUNDS.width}x${BOUNDS.height}`;
} catch (e) {
  statusEl.textContent = `JS init failed: ${e}`;
}

const sx = cv.width / BOUNDS.width;
const sy = cv.height / BOUNDS.height;

meta.textContent = [
  `Scale: ${sx.toFixed(3)} x ${sy.toFixed(3)}`,
  `This is region replay (not pixel replay)`,
  `Legend: blocks show where draw_copy updated`,
  `Colors by type: GlzRgb=cyan, LzRgb=green, Quic=orange, others=magenta`,
].join('\\n');

function colorForType(t) {
  if (t === 'GlzRgb') return 'rgba(0,170,255,1.0)';
  if (t === 'LzRgb') return 'rgba(0,200,120,1.0)';
  if (t === 'Quic') return 'rgba(255,140,0,1.0)';
  return 'rgba(200,0,200,1.0)';
}

let idx = 0;
let timer = null;

function clearCanvas() {
  ctx.fillStyle = '#f8fafc';
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.strokeStyle = 'rgba(0,0,0,0.08)';
  for (let x = 0; x < cv.width; x += 40) {
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, cv.height);
    ctx.stroke();
  }
  for (let y = 0; y < cv.height; y += 40) {
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(cv.width, y);
    ctx.stroke();
  }
}

function drawEvent(e) {
  const x = (e.x - BOUNDS.min_x) * sx;
  const y = (e.y - BOUNDS.min_y) * sy;
  const w = Math.max(1, e.w * sx);
  const h = Math.max(1, e.h * sy);
  ctx.fillStyle = colorForType(e.type);
  ctx.fillRect(x, y, w, h);
  ctx.strokeStyle = 'rgba(0,0,0,0.95)';
  ctx.strokeRect(x, y, w, h);

  evtBox.textContent = [
    `#${idx}/${EVENTS.length}`,
    `ts=${e.ts}`,
    `surface=${e.surface}`,
    `type=${e.type}`,
    `pos=(${e.x},${e.y}) size=${e.w}x${e.h}`,
    `data_bytes=${e.data_bytes}`,
    `id=${e.id}`,
  ].join('\\n');
  progress.textContent = `${idx}/${EVENTS.length}`;
}

function stepOnce() {
  if (idx >= EVENTS.length) return false;
  const e = EVENTS[idx++];
  drawEvent(e);
  return true;
}

function play() {
  if (idx >= EVENTS.length) {
    idx = 0;
    clearCanvas();
  }
  if (timer) return;
  statusEl.textContent = 'Playing...';
  timer = setInterval(() => {
    if (!stepOnce()) {
      clearInterval(timer);
      timer = null;
      statusEl.textContent = 'Finished';
    }
  }, 1000 / Number(speed.value));
}

function pause() {
  if (timer) {
    clearInterval(timer);
    timer = null;
  }
}

document.getElementById('play').onclick = play;
document.getElementById('pause').onclick = pause;
document.getElementById('step').onclick = () => stepOnce();
document.getElementById('clear').onclick = () => { pause(); idx = 0; clearCanvas(); evtBox.textContent = ''; };
speed.oninput = () => {
  speedv.textContent = speed.value;
  if (timer) { pause(); play(); }
};

clearCanvas();
if (EVENTS.length > 0) {
  statusEl.textContent = 'Ready';
} else {
  statusEl.textContent = 'No draw_copy events parsed from log';
}
</script>
</body>
</html>
"""


def write_html(out_path: Path, events, bounds):
    html = HTML_TMPL.replace("__EVENTS__", json.dumps(events)).replace(
        "__BOUNDS__", json.dumps(bounds)
    )
    html = html.replace("__EVENT_COUNT__", str(len(events)))
    html = html.replace("__BOUNDS_TEXT__", f"{bounds['width']}x{bounds['height']}")
    out_path.write_text(html, encoding="utf-8")


def main():
    ap = argparse.ArgumentParser(description="Replay SPICE draw_copy log as animation")
    ap.add_argument("log", type=Path, help="ryll log file")
    ap.add_argument(
        "-o",
        "--output",
        type=Path,
        default=Path("draw_copy_replay.html"),
        help="output HTML file",
    )
    args = ap.parse_args()

    events, bounds = parse_events(args.log)
    write_html(args.output, events, bounds)
    print(f"Wrote {args.output} with {len(events)} events, bounds={bounds['width']}x{bounds['height']}")


if __name__ == "__main__":
    main()
