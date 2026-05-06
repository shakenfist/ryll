# Phase 8: Operator docs + systemd + TLS recipes

## Prompt

Before responding to questions or making changes, read the
master plan at `docs/plans/PLAN-web-frontend.md` (Phase 8
section in the Execution table and prose), and the current
state of the operator-facing docs:

- `docs/web-frontend.md` — already covers Quick start, What
  works, Reconnect behaviour, Limitations, Security note,
  CI smoke test, Pending phases. Phase 8 fills the gaps.
- `README.md` — current `--web` description includes a
  pointer to `docs/web-frontend.md` and notes that
  packaging / docs are 0–7/8 complete with Phase 8 listed
  as pending.
- `ARCHITECTURE.md` — Phase 6/7 paragraphs already exist;
  Phase 8 may add a small operator-facing pointer.
- `AGENTS.md` — `--web` is in the modes list.
- `docs/portability.md` — `--web` is verified Linux-only.

Cross-reference: the master plan calls out
`kerbside/docs/` as a place to consider adding a brief
mention of ryll's `--web` mode as a deployment pattern.
Kerbside is a separate repo at
`/srv/kasm_profiles/mikal/vscode/src/shakenfist/kerbside/`.

External references (no need to read upfront, but useful
during implementation): systemd unit syntax (man
`systemd.unit`, `systemd.service`); Caddy automatic-HTTPS
docs; nginx `proxy_pass` patterns; Let's Encrypt /
certbot; mkcert for local dev; `Type=simple` vs
`Type=notify` for graceful shutdown.

Flag any uncertainty rather than guessing.

## Goal

Close the operator-facing documentation gap so a sysadmin
who does not know the codebase can:

- Run `ryll --web` as a long-lived systemd service.
- Front it with TLS via Caddy or nginx, including an
  end-to-end recipe for obtaining a cert (LAN-only via
  mkcert; public via Let's Encrypt).
- Diagnose common failure modes (no video, ICE failure,
  no audio, autoplay blocked) without reading source.
- Know where ryll's `--web` mode fits in the broader
  shakenfist deployment story (the kerbside cross-ref).

After Phase 8:

- `docs/web-frontend.md` has a Service mode, Troubleshooting,
  and TLS sections, plus a deployment-patterns appendix.
- A reference systemd unit lives at `examples/ryll-web.service`
  (or under `packaging/`, wherever existing examples live).
- A reference Caddyfile and nginx server-block live next to
  the unit file.
- `kerbside/docs/` has a one-paragraph note pointing at
  ryll's `--web` mode for browser-based SPICE access.
- The master plan's Phase 8 row flips to Complete; README
  and index update to "Phases 0–8 of 8 complete" or
  equivalent (the web-frontend project becomes Complete).

Out of scope:

- **Native TLS in ryll's axum server.** The plan defaults
  to documenting the reverse-proxy pattern (Caddy/nginx).
  Native TLS is a code change, not a doc change, and is
  flagged as an Open Question below for user decision.
- Multi-viewer support, auth integration (OIDC, SAML),
  audit logging, rate limiting — all future work.
- A Docker / Podman container image for ryll-as-a-service.
  Could be a follow-up after Phase 8 if the operator wants
  it, but not required for MVP completion.
- Distro-specific packaging tweaks beyond what Phase 7
  already shipped.

## Open question for user before execution

**Does Phase 8 include native TLS in axum, or is the
reverse-proxy recipe sufficient?**

- **Doc-only path** (default): Phase 8 documents how to
  put Caddy or nginx in front of ryll. Caddy auto-fetches
  certs; nginx + certbot is the manual path. ~5 LoC of
  unit-file glue, no Rust changes.
- **Native TLS path**: ~80–120 LoC of Rust changes.
  Add `--web-tls-cert`/`--web-tls-key` flags. Use
  `axum-server` with `RustlsConfig`. Adds an extra config
  surface and a small ongoing maintenance load. The
  WebRTC layer already does its own DTLS-SRTP — adding
  TLS to the signalling page is purely about not leaking
  the URL token over plain HTTP between browser and
  server.

The doc-only path is recommended for MVP because:

1. Operators who care about TLS termination probably
   already run a reverse proxy for other services.
2. Caddy's auto-TLS makes the reverse-proxy story
   genuinely two-line.
3. Native TLS adds a config surface that can drift from
   the axum/rustls version pinning.

If the user picks the native-TLS path, the plan grows by
one extra commit (8d′ or similar). The plan as written
defaults to doc-only and flags this for confirmation.

## Scope

In:

- `docs/web-frontend.md` — extend with:
  - **Service mode** section: systemd unit file,
    user/group recommendations, `EnvironmentFile`
    pattern for the `.vv` path, `Restart=on-failure`,
    `KillSignal=SIGTERM` (Phase 6's graceful shutdown),
    log capture via `StandardOutput=journal`.
  - **TLS via reverse proxy** section: Caddy two-line
    recipe, nginx server-block recipe, where to find the
    upstream port (the URL ryll prints).
  - **Cert recipes**: mkcert for LAN dev, Let's Encrypt
    via Caddy auto, certbot for nginx.
  - **Troubleshooting**: no video; ICE failure (NAT
    traversal hints; STUN/TURN not used in MVP);
    no audio (autoplay policy; PCM-only servers);
    "Click to reconnect" loops; Ctrl-C ignored
    (already-shipped fix in Phase 6 — note as historic
    context).
  - **Deployment patterns** appendix: localhost-only,
    LAN-only, behind-Caddy with public DNS, behind-nginx
    in an existing reverse proxy.
- `examples/ryll-web.service` — reference systemd unit.
- `examples/Caddyfile` — reference Caddy config.
- `examples/nginx-ryll-web.conf` — reference nginx server
  block.
- `kerbside/docs/index.md` (or capabilities.md / proxy-
  architecture.md, whichever fits) — one-paragraph note
  pointing at ryll `--web` for browser access.
- `README.md` — flip the multi-modal table from "0–7 of
  8 complete" to "Complete (8/8)" or similar; bump the
  `--web` row from "In progress" / "Pending" to
  "Shipping".
- `docs/plans/PLAN-web-frontend.md` — Phase 8 row flipped
  to Complete; project status flips to Complete.
- `docs/plans/index.md` — web-frontend project marked
  Complete.

Out:

- All items in "Out of scope" above.
- Reorganising the existing `docs/web-frontend.md` —
  append, don't refactor.
- A Container/Docker recipe (deferred).

## Approach

### Service mode (8a)

systemd unit at `examples/ryll-web.service`:

```ini
[Unit]
Description=ryll --web SPICE-to-browser transcoder
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ryll
Group=ryll
EnvironmentFile=/etc/ryll/web.env
# web.env declares: VV_FILE=/etc/ryll/session.vv WEB_HOST=0.0.0.0 WEB_PORT=8080
ExecStart=/usr/bin/ryll --web --file ${VV_FILE} --web-host ${WEB_HOST} --web-port ${WEB_PORT}
Restart=on-failure
RestartSec=5s
KillSignal=SIGTERM
TimeoutStopSec=10s
StandardOutput=journal
StandardError=journal

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadOnlyPaths=/etc/ryll
# ryll opens UDP for ICE; allow UDP egress.
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX

[Install]
WantedBy=multi-user.target
```

The `web-frontend.md` "Service mode" section explains:
- Where to put the .vv file (`/etc/ryll/session.vv`,
  readable only by the ryll user).
- How to capture and rotate the per-launch URL/token —
  it's printed to stdout, so journalctl is the answer
  (`journalctl -u ryll-web -n 1 | grep -o 'http://.*'`).
- That `KillSignal=SIGTERM` is required (not the default
  for Type=simple is SIGTERM, but documenting explicitly
  protects against accidental override) so Phase 6's
  graceful-shutdown path engages.

### TLS via reverse proxy (8b)

Caddyfile (the canonical recipe — autocert handles the
cert lifecycle):

```caddy
ryll.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

That's the entire config for a publicly-reachable
deployment with an A record pointing at the host. Caddy
talks to Let's Encrypt automatically.

For LAN-only or self-signed:

```caddy
ryll.lan:8443 {
    tls /etc/ssl/ryll.crt /etc/ssl/ryll.key
    reverse_proxy 127.0.0.1:8080
}
```

nginx equivalent (for sysadmins who already run nginx):

```nginx
server {
    listen 443 ssl http2;
    server_name ryll.example.com;

    ssl_certificate     /etc/letsencrypt/live/ryll.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/ryll.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        # No upgrade/Connection headers needed: WebRTC negotiates
        # out-of-band over UDP after the initial /offer POST.
        proxy_buffering off;
    }
}
```

**Important note** for both: the WebRTC media path is
NOT proxied. ICE candidates from ryll point at ryll's
host and port directly; the browser opens a UDP flow
to that endpoint. The reverse proxy carries only the
HTTP signalling page + `/offer` POST. This means:

- ryll's UDP port range must be reachable from the
  browser (firewall holes for the ephemeral RTP ports).
- Bind ryll's `--web-host` to the public-facing IP, not
  to 127.0.0.1, when behind a proxy that itself listens
  on a different IP.

This caveat is the section's most important takeaway
and goes in a callout box in `docs/web-frontend.md`.

### Cert recipes (8b cont.)

- **Let's Encrypt via Caddy**: nothing to do; Caddy
  fetches and renews. Document the prerequisite (DNS A
  record, port 80/443 reachable).
- **Let's Encrypt via certbot + nginx**:
  `certbot --nginx -d ryll.example.com`. Assumes nginx
  is already serving the domain on port 80.
- **mkcert for LAN dev**:
  ```
  mkcert -install
  mkcert ryll.lan 192.168.1.10
  ```
  Drop the resulting `.pem` files into the Caddyfile or
  nginx config. Trust is on the dev's machine via
  mkcert's local CA.
- **Self-signed for one-off use**:
  ```
  openssl req -x509 -newkey rsa:2048 -keyout key.pem \
    -out cert.pem -days 30 -nodes -subj "/CN=ryll.lan"
  ```
  Browser will show a warning — acceptable for one-off
  diagnostic access.

### Troubleshooting (8b cont.)

Section structure: symptom → likely cause → fix.

- **Page loads, video stays black for >10 seconds**:
  - Check browser console for `RTCPeerConnection`
    state. If stuck on `connecting` → ICE failure (UDP
    blocked between browser and ryll). If on
    `connected` but no frames → encoder didn't start
    (check ryll's stderr).
  - Resolution: the encoder requests a keyframe on
    `Connected`; the very first frame can take up to
    1 second. If beyond that, encoder is wedged —
    file a bug.
- **No audio, video works**:
  - Browser autoplay policy: click the volume button
    on the page. The `<video>` is muted by default to
    satisfy the policy.
  - PCM-only SPICE server: ryll only does Opus
    passthrough in MVP. Server logs will say
    "playback channel negotiated PCM; web mode is
    silent until a future PCM→Opus encoder lands".
- **"Click to reconnect" loop**:
  - 5 attempts then manual button per Phase 6. If the
    server is alive but no offer is being accepted,
    check that the reaper is consuming the dead
    signal (logs: `bridge reaper: bridge died,
    reaping`). If the reaper is stuck, Ctrl-C and
    restart.
- **High CPU when no browser is connected**:
  - Phase 6 made the reaper proactive; if you see this
    on Phase 6+ ryll, the reaper isn't running or the
    bridge isn't reaching a terminal state. Check
    logs and file a bug.
- **Ctrl-C ignored**:
  - Pre-Phase-6 only — Phase 6's `with_graceful_shutdown`
    fixed this. Update to ryll ≥ Phase 6.

### kerbside cross-reference (8c)

Find the right home in `kerbside/docs/`. Likely
candidates: `index.md` (a "Related projects" or
"Console sources" pointer) or `proxy-architecture.md`
("ryll exposes the same SPICE channels but with a
browser frontend via WebRTC; useful when the operator
wants browser access without going through a separate
RDP / Guacamole stack"). Pick whichever fits the
existing structure best.

A single paragraph, ~3 sentences, with a link back to
`https://github.com/shakenfist/ryll` and the relevant
section of `docs/web-frontend.md`.

### Status flips (8d)

- `docs/plans/PLAN-web-frontend.md` — Phase 8 row
  Complete. Master plan project status: Complete.
- `docs/plans/index.md` — web-frontend row status:
  Complete.
- `README.md` — bump from "0–7/8" to "Complete";
  promote the `--web` mode from in-progress to
  shipping in the multi-modal table.
- `ARCHITECTURE.md` — short sentence in the multi-modal
  section noting that all eight phases shipped.

## Prerequisites

- Phase 7 complete on `thought-bubble`. (It is — last
  commit `5d14b053`.)
- User decision on the **Open question** (native TLS
  vs doc-only). The plan defaults to doc-only.

## Steps

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 8a   | medium | sonnet | none     | Service mode. Add `examples/ryll-web.service` (systemd unit per the plan). Add a "Service mode" section to `docs/web-frontend.md` covering installation, the EnvironmentFile pattern, journalctl URL extraction, and how Phase 6's graceful shutdown integrates with `KillSignal=SIGTERM`. Single commit. |
| 8b   | medium | sonnet | none     | TLS via reverse proxy + cert recipes + troubleshooting. Add `examples/Caddyfile` and `examples/nginx-ryll-web.conf`. Add a "TLS via reverse proxy", "Cert recipes", and "Troubleshooting" section to `docs/web-frontend.md`. Include the WebRTC-media-not-proxied callout prominently. Single commit. |
| 8c   | low    | sonnet | none     | kerbside cross-reference. Open `/srv/kasm_profiles/mikal/vscode/src/shakenfist/kerbside/docs/`, pick the right file (most likely `index.md` or `proxy-architecture.md`), add a one-paragraph pointer to ryll `--web` mode. Commit in the kerbside repo (separate git context). Single commit. |
| 8d   | medium | sonnet | none     | Status flips + README + ARCHITECTURE polish. Flip Phase 8 + project status in master plan, index, README. Add a brief paragraph to ARCHITECTURE.md noting the project landed end-to-end. Single commit. |

After 8d, Phase 8 is done and the web-frontend project is
**Complete**.

## Step details

### Step 8a expanded brief

The systemd unit needs:

- `Type=simple` (matches ryll's blocking-foreground main).
- `KillSignal=SIGTERM` (default but document explicitly).
- `TimeoutStopSec=10s` (Phase 6's shutdown drains within
  ~5s; 10s is generous).
- `Restart=on-failure` (auto-recover from crash; do NOT
  restart on success — operators should explicitly stop
  the service).
- Hardening: `NoNewPrivileges`, `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `ReadOnlyPaths=/etc/ryll`,
  `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`.

EnvironmentFile pattern keeps the `.vv` path and
host/port out of the unit file so operators can edit
config without touching systemd. Document the example
`/etc/ryll/web.env`:

```
VV_FILE=/etc/ryll/session.vv
WEB_HOST=0.0.0.0
WEB_PORT=8080
```

Document journalctl recipe for token extraction:

```bash
journalctl -u ryll-web -n 50 --no-pager | grep -oE 'http://[^ ]+token=[^ ]+' | tail -1
```

### Step 8b expanded brief

The TLS section's most important content is the
WebRTC-media-not-proxied callout. Make it prominent
(blockquote / admonition / bold call-out). Operators
who don't read this will hit "page loads, video black"
because their proxy doesn't forward the UDP RTP flow.

Caddy recipe is two lines because Caddy handles the
cert lifecycle automatically. nginx recipe is longer
because the operator needs to obtain the cert separately
(certbot or manual).

Cert recipes: keep each to ~5 lines. mkcert is the
LAN-dev pattern; certbot is the public-DNS pattern;
self-signed is the "I just need it to work for one
afternoon" pattern.

Troubleshooting: each entry follows the same shape:
**Symptom** in bold, then a short bulleted list of
likely causes with concrete logs to check. Don't write
essays.

### Step 8c expanded brief

Read `kerbside/docs/index.md` and
`kerbside/docs/proxy-architecture.md` to choose the
right file. The note should:

- Be ~3 sentences.
- Mention that ryll's `--web` mode is a browser
  frontend for SPICE that does not require kerbside,
  but kerbside operators may find it useful for
  internal access scenarios.
- Link to `https://github.com/shakenfist/ryll/blob/main/docs/web-frontend.md`.

Commit message in the kerbside repo follows kerbside's
conventions (check that repo's recent commit log).
Use the standard Co-Authored-By + Signed-off-by
trailers.

### Step 8d expanded brief

Mechanical doc updates:

- `docs/plans/PLAN-web-frontend.md`: Phase 8 row →
  Complete with the four 8a–8d commit SHAs. Master
  plan's overall project status (top of the file or
  wherever it lives) → Complete.
- `docs/plans/index.md`: web-frontend project status
  → Complete.
- `README.md`: flip the multi-modal table's web row to
  Shipping. Update progress to "All 8 phases complete"
  or similar.
- `ARCHITECTURE.md`: short sentence in the multi-modal
  section confirming `--web` mode shipped.

Don't write essays. The reconnect/CI prose from 6d/7e
is the right tone.

## Acceptance criteria

- `make lint` and `make test` pass after each step (no
  Rust changes expected, but verify).
- After 8a: `examples/ryll-web.service` exists and parses
  cleanly with `systemd-analyze verify` (run locally if
  available; the syntax is mechanical and the agent can
  cross-check against `man systemd.service`).
- After 8b: `examples/Caddyfile` validates with
  `caddy validate` (if the agent has caddy locally;
  otherwise visual review). nginx config is syntactically
  valid (`nginx -t -c <path>` or visual review).
- After 8c: kerbside repo has a new commit referencing
  ryll `--web`.
- After 8d: master plan, index, README all flip to
  Complete.
- `pre-commit run --all-files` passes after each commit
  in the ryll repo.

## Risks

- **WebRTC-not-proxied caveat being missed.** This is the
  single highest-impact risk in the troubleshooting
  story. Make it prominent.
- **systemd unit hardening too aggressive.**
  `ProtectSystem=strict` + `ReadOnlyPaths=/etc/ryll`
  works for the common case but blocks an operator who
  wants to write logs to disk via `--log-file`. If the
  user uses --log-file routinely, relax `ReadWritePaths`
  appropriately. Document the constraint.
- **kerbside repo conventions.** This plan does NOT
  bundle the kerbside change with the ryll commits.
  Each repo gets its own commit; the kerbside change is
  pushed separately. Sub-agent for 8c works in the
  kerbside checkout.
- **Native TLS deferred.** If user wants native TLS, the
  plan grows by one step (8b′ or similar). Flagged as
  Open Question.
- **Existing TLS reference in `docs/web-frontend.md`**
  ("wait for Phase 8's TLS support") needs editing to
  match the doc-only path. 8b should rewrite that
  paragraph.
- **README "TLS support" line** at line 26 references
  inline CA certs from `.vv` files — a separate feature
  unrelated to web-mode TLS. Leave alone.

## Documentation updates

After 8d:

- `docs/web-frontend.md` extended with Service mode,
  TLS, Cert recipes, Troubleshooting, Deployment
  patterns.
- `examples/ryll-web.service`, `examples/Caddyfile`,
  `examples/nginx-ryll-web.conf` created.
- `docs/plans/PLAN-web-frontend.md` Phase 8 → Complete;
  project Complete.
- `docs/plans/index.md` web-frontend project → Complete.
- `README.md` `--web` mode → Shipping.
- `ARCHITECTURE.md` web-frontend section → "shipped".
- `kerbside/docs/<file>.md` — ryll `--web` cross-ref.

## Estimated total scope

~600–900 lines across four commits. Heaviest in 8b
(~300 LoC of docs + two example config files) and 8a
(~150 LoC unit file + docs section). 8c is ~30 LoC in
the kerbside repo. 8d is ~80 LoC of status flips and
prose polish.

## Back brief

Before executing 8a, the implementing agent should
back-brief: which directory examples live in (look for
existing `examples/`, `packaging/`, or `etc/` directories
to match the repo's convention), and confirm whether
`Type=simple` matches ryll's actual blocking behaviour
(yes — main.rs's `runtime.block_on` blocks the main
thread).

For 8b, agent should back-brief on whether to put the
Caddyfile and nginx config under `examples/` or some
other dir if the repo prefers (e.g., `packaging/` for
.deb/.rpm-related config; `examples/` for operator
recipes is more natural).

For 8c, the agent should back-brief which kerbside
file is the most natural home for the cross-ref before
editing.
