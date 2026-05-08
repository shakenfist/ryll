# Phase 02: Main-channel reconnect / keepalive (K1 fix)

## Prompt

Before answering questions or making design decisions in this
document, read the relevant ryll source. Key files:
`shakenfist-spice-renderer/src/channels/main_channel.rs` (the
client-side 30 s keepalive timeout and the PING / PONG path),
`shakenfist-spice-protocol/src/client.rs` (TCP keepalive socket
options applied at connect time), `ryll/src/app.rs` (the
existing manual `reconnect()` method and the `ChannelEvent`
handlers extended in Phase 01), and `ryll/src/bugreport.rs`
(the `DisconnectCause` record produced at the moment of
failure). Consult `ARCHITECTURE.md` for channel and event flow,
`AGENTS.md` for build and test conventions, and the SPICE
reference at `/srv/src-reference/spice/` for the server's rcc
liveness check (`spice/server/red-channel-client.cpp:656` and
`main-channel-client.cpp:38` for the 30 s constant) and
spice-gtk's keepalive strategy (`spice-gtk/src/spice-session.c:2286`
TCP keepalive setup, `spice-gtk/src/channel-base.c:43` reactive
PONG).

This phase lands the user-visible fix for bug **K1** — "main
channel rcc 30 s unresponsive timeout tears down session" —
identified during dogfooding session 001. It is gated on
**Phase 01 data**: at least one disconnect-cause.json zip
captured from a real reproduction. Without that, the diagnostic
branches under "Approach" cannot be selected, and we would be
designing the fix from speculation. See "Prerequisite" below.

One commit per logical step (no-regret pieces independent of
the diagnostic outcome can land before the data arrives, but
the conditional branches must wait). Each commit must build,
lint, and pass tests on its own.

## Situation

### What we already established

**Server-side timeout is 30 s, not 300 s** (Q2 from the master
plan, resolved). At
`/srv/src-reference/spice/spice/server/main-channel-client.cpp:38`:

```cpp
#define CLIENT_CONNECTIVITY_TIMEOUT (MSEC_PER_SEC * 30)
```

The check itself lives in
`/srv/src-reference/spice/spice/server/red-channel-client.cpp:656`
(`connectivity_timer`), measures **any inbound byte** from the
client, and resets on receive. If 30 s pass with no byte
received, the server logs `"rcc has been unresponsive for more
than %u ms"` and tears down the session. The user perceives
this as an inputs-channel disconnect because the entire SPICE
session drops when main is torn down.

**TCP keepalive is already configured on the SPICE socket** at
`shakenfist-spice-protocol/src/client.rs:189-202`. Values match
spice-gtk exactly: `TCP_KEEPIDLE = 30 s`, `TCP_KEEPINTVL = 15 s`,
`TCP_KEEPCNT = 3`. This rules out "we forgot the obvious thing".

**ryll responds to server PINGs sub-millisecond** in every
session-001 pcap (verified during triage). The PING handler at
`main_channel.rs:522-563` is purely synchronous on the channel
read loop — `Ping::read()` parses, the PONG payload is built,
and `make_message()` queues it on the send loop, all without
awaiting anything that could block.

**Client-side mirror timeout** at
`shakenfist-spice-renderer/src/channels/main_channel.rs:297-311`
fires after 30 s of no inbound data on main:

```rust
_ = tokio::time::sleep_until(last_data_received + keepalive_timeout) => {
    info!("main: no data received for {}s, assuming disconnected", ...);
    if let Ok(mut snap) = self.snapshot.lock() {
        snap.keepalive_timeout_fired = true;
    }
    self.event_tx
        .send(ChannelEvent::Disconnected(ChannelType::Main))
        .await
        .ok();
    self.repaint_notify.notify_one();
    break;
}
```

This is the **same 30 s window as the server's rcc check**, so
either side firing first triggers a teardown. The Phase 01
disconnect-cause.json record now distinguishes "we timed
ourselves out" (`keepalive_timeout_fired = true`) from a real
EOF / RST.

**Reconnect today is manual.** `RyllApp::reconnect()` at
`app.rs:701` is wired only to the "Reconnect" button on the
disconnect modal at `app.rs:3127`. There is no auto-retry, no
backoff, no surface for non-modal reconnect attempts. The
`connection_cancel: Option<Arc<AtomicBool>>` plumbing
(`app.rs:419`, `app.rs:706`, `app.rs:787`) is reusable for an
auto-retry path — we cancel the previous attempt and spawn a
new one, exactly the same way the manual button does today.

**spice-gtk and virt-viewer have no application-layer
keepalive.** They rely on TCP keepalive (`spice-session.c:2286`,
identical values) plus reactive PONG. If TCP keepalive +
reactive PONG were sufficient, ryll would not see this
disconnect — yet it does, on macOS, with the user actively
using their computer. So either ryll is doing something
spice-gtk doesn't, or the platform is doing something to ryll
that it doesn't do to virt-viewer. Both are testable from the
disconnect-cause.json + pcap once Phase 01 data is in hand.

### What we don't yet know

The session-001 pcaps captured only post-reconnect activity —
they do not contain the moment of failure. We cannot tell:

1. Whether the server stopped sending PINGs in the seconds
   before disconnect (server-side starvation).
2. Whether ryll's tokio runtime stopped processing reads in
   time (client-side starvation — most plausibly macOS App Nap
   when ryll is not the foreground window).
3. Whether the TCP path itself silently dropped traffic between
   the OS keepalive probe interval (rare but possible on flaky
   wifi / VPN).

Phase 01's `disconnect-cause.json` plus the disconnect-moment
pcap will resolve this. The diagnostic decision tree under
"Approach" branches accordingly.

## Mission and problem statement

Make ryll survive the K1 disconnect class without the user
having to click "Reconnect", and where possible **prevent** the
disconnect from happening at all. The phase has two halves:

1. **No-regret UX**: introduce automatic reconnect with backoff
   on transport failure, so a momentary disconnect (network
   blip, server restart, ticket reuse on the same gateway) is
   recovered transparently. This applies regardless of the K1
   root cause.

2. **Root-cause fix for K1**: diagnose against Phase 01 data,
   then apply the matching one of three pre-designed fixes.

The phase succeeds when:

- A user who leaves a SPICE session running on macOS overnight
  (or while doing other work for >30 minutes) returns to a
  still-functional session, OR returns to a session that
  reconnected automatically without manual intervention.
- The next dogfooding session does not reproduce K1, OR if it
  does, the produced disconnect-cause.json + pcap show a path
  we know how to address (rather than the speculative state we
  are in today).
- No regression for the ticket-bounded deployments (Kerbside,
  oVirt) where reconnect against a one-time ticket is doomed —
  ryll detects these via the standard `delete-this-file=1`
  console.vv key and shows an explanatory modal instead of
  retrying. See §A.4.
- A new console.vv extension `ticket-valid-until=<unix-ts>`
  is parsed and surfaced (countdown UI, expiry-aware modal,
  pre-expiry warning notification). Documented in the
  companion `console-vv-extensions.md` doc — see "Companion
  docs" below. Producers (Kerbside, oVirt) are not yet
  emitting this key on day one; the absence is a no-op.

## Prerequisite

**Phase 02 implementation is gated on at least one
`disconnect-cause.json` zip from a real K1 reproduction.** That
zip must:

- Have `keepalive_timeout_fired` set on the `main` snapshot (or
  explicitly *not* set, in which case the cause is server-side
  RST and the diagnostic branch is different).
- Carry a `traffic.pcap` whose end shows the run-up to the
  failure — last 60 s of main-channel traffic before the
  timeout.
- Be reproducible (the user has been able to trigger K1 just
  by leaving the session idle while using the host for other
  work; a ~30 minute idle window has been sufficient on
  session-001).

The no-regret UX work (auto-reconnect with backoff, sections
"Auto-reconnect" below) **may proceed in parallel** with data
collection — it does not depend on the diagnostic outcome.

## Approach

The work breaks into three blocks. Block A is no-regret and can
land first. Block B is the diagnostic step (no code, just
analysis). Block C is the conditional fix selected by Block B.

### Block A — Auto-reconnect with backoff (no-regret)

Today, every disconnect terminates in the modal at
`app.rs:3119`. The user clicks "Reconnect", which calls
`RyllApp::reconnect()` (`app.rs:701`). Block A inserts an
automatic retry layer between the disconnect signal and the
modal.

#### A.1 Retry policy

Three attempts with exponential backoff: **1 s, 4 s, 16 s**
(matching the spice-gtk `SPICE_SESSION_PROPS_PROTOCOL` retry
shape — short first attempt for blip recovery, longer windows
for server restarts). Total worst-case wait ~21 s before the
modal pops.

Caps:

- Maximum 3 attempts per disconnect cluster; subsequent
  disconnects within a 5 minute window do not extend the
  budget. (Otherwise a flapping server would have ryll banging
  away forever.)
- Auto-reconnect **does not** trigger when the .vv said
  `delete-this-file=1` (single-use ticket — see §A.4) or when
  `ticket-valid-until` has elapsed (§A.5) — both are known-
  doomed retries.

#### A.2 Wiring

A new state machine on `RyllApp`:

```rust
enum ReconnectState {
    Idle,                        // connected normally
    Pending { attempt: u8, next_at: Instant },
    Modal,                       // budget exhausted, user takes over
}
```

Replaces the bool-ish `show_disconnect_dialog`. Driven from
the existing GUI tick loop (`update()` in `app.rs`). When
`ChannelEvent::Disconnected` / `Error` fires, transition Idle →
Pending{1, now+1s}. The tick loop checks if `next_at` has
passed and triggers a `reconnect()` if so. On success, back to
Idle. On failure, increment attempt; if attempt > 3 or budget
exhausted, transition to Modal (current behaviour).

The disconnect-snapshot logic from Phase 01 still runs at the
event handler — auto-reconnect does not suppress it. Each
attempt that fails *also* writes a snapshot, subject to the
existing 60 s cooldown (which was designed for exactly this
case).

#### A.3 UI surface

Two visible changes:

1. **Status-bar indicator** — when in `Pending`, show
   "Reconnecting… (attempt 2/3)" in the bottom status panel
   beside the existing FPS/connected widgets. Dismiss on
   success or on Modal transition.
2. **Notification** — push a `NotifySeverity::Warn` entry per
   attempt failure with source `NotificationSource::BugReport`
   ("Reconnect attempt 2 failed: <error>"). Same notification
   plumbing Phase 01 already uses for "Disconnect snapshot
   saved to …".

Modal copy varies by exit cause — see A.6 below.

#### A.4 Detecting one-shot tickets via `delete-this-file`

In Kerbside / oVirt deployments, the SPICE ticket is a
one-time-use token: once any channel has linked with it, the
server invalidates it. A reconnect attempt with the same
ticket fails at `reds.cpp:2098-2110`'s ticket-validation step.

We **must not auto-reconnect in that case** — it produces a
ratchet of failed attempts, each writing a snapshot (despite
cooldown bounding it), confusing the user and the reviewer of
the bug-report directory.

**The standard virt-viewer `delete-this-file=1` key is a
reliable proxy for one-shot ticket semantics.** Empirically
every producer that emits one-shot tickets (Kerbside, oVirt)
also sets `delete-this-file=1`, because the file becomes
useless after the first link establishment. Reusable-ticket-
with-`delete-this-file=1` is a deployment contradiction (what
would you reuse from after deletion?). The spec does not
formally require this interpretation, but the empirical
contract is strong enough to lean on.

Implementation: extend the .vv parser at
`ryll/src/config.rs:266` to read `delete-this-file` and
surface it on `Config` as a new `bool` field
(`ticket_is_single_use`). When `true`, the auto-reconnect
state machine refuses to enter `Pending` — disconnects go
straight to `Modal { variant: OneShotConsumed }`.

Does **not** add a new CLI flag or a new console.vv key —
piggybacks on a key that exists, so day-one behaviour against
existing producers (Kerbside, oVirt) is correct without
producer-side changes. If a future producer ever wants
file-deletion-without-single-use semantics, an explicit
override key can be added then; speculatively defining one now
just invents a contradiction nobody asked for.

This interpretation is documented prominently in the README
and in the new `console-vv-extensions.md` doc (see "Companion
docs" below) so producers know what we infer from the standard
key.

#### A.5 Ticket validity window via `ticket-valid-until`

A new console.vv extension key:

```ini
[virt-viewer]
ticket-valid-until=1730500000  ; unix timestamp
```

Optional. When set, ryll knows when the server will reject
the ticket regardless of one-shot status. Three uses:

1. **Auto-reconnect bound.** `ReconnectState::Pending` checks
   `now() >= ticket_valid_until` before each attempt; if past
   expiry, transitions to `Modal { variant: TicketExpired }`
   instead of retrying.
2. **Pre-disconnect warning.** A `NotifySeverity::Warn`
   notification fires once at T-30 s relative to expiry:
   "Session ticket expires in 30 seconds." Driven from the
   GUI tick loop, not a dedicated timer.
3. **Modal context.** `Modal { variant: TicketExpired }`
   includes the expiry timestamp in the body text.

This is a genuinely new extension — no existing console.vv
key carries this information. Document under "extensions" in
the new doc; raise as an RFE against Kerbside (in
`/home/mikal/src/shakenfist/kerbside`) and against oVirt
issue tracker once the doc lands.

Day-one behaviour with no producers populating the key:
identical to today (key absent → no expiry tracking → no
behaviour change beyond A.4's `delete-this-file` reading).

#### A.6 Disconnect modal variants

`ReconnectState::Modal` carries a variant discriminant:

```rust
enum ModalVariant {
    Generic { latest_error: String },     // generic disconnect, retry possible
    OneShotConsumed,                      // delete-this-file=1 was set
    TicketExpired { expired_at: SystemTime }, // ticket-valid-until elapsed
}
```

UI rendered at `app.rs:3119`:

| Variant | Title | Body | Buttons |
|---|---|---|---|
| Generic | "Connection lost" | "Three automatic reconnect attempts failed: \<latest_error\>." | Reconnect, Quit |
| OneShotConsumed | "Session ended — cannot reconnect" | "This connection used a single-use ticket. Request a new connection from the system that issued the original link." | Quit only |
| TicketExpired | "Session ended — ticket expired" | "The ticket for this session expired at \<HH:MM:SS\>. Request a new connection." | Quit only |

Both `OneShotConsumed` and `TicketExpired` omit the Reconnect
button — there is no useful action for the user inside ryll;
the doomed-retry ratchet is exactly what the variant exists to
prevent.

Edge case: `ticket-valid-until` set but in the future at
disconnect time. The server told us the ticket expired but our
clock thinks it's still valid — almost certainly clock skew.
Render the `TicketExpired` modal anyway (server's view is
authoritative) but log a `warn!` "ticket-valid-until in the
future at disconnect time, possible clock skew" so future
debugging has a hook.

#### A.7 Reset path

#### A.7 Reset path

`reconnect()` at `app.rs:701` already does the right teardown
(cancel previous, clear surfaces, respawn). One adjustment:
also clear the `keepalive_timeout_fired` flag on the
`MainSnapshot` so a subsequent disconnect cleanly reports its
own cause. Phase 01's open-question 3 listed this as the
right fix; do it now in `reconnect()` rather than scattering
clearing logic. If the `MainSnapshot` already exists at the
point `reconnect()` runs (it does, via
`self.channel_snapshots.main`), this is a one-liner.

### Block B — Diagnostic step (no code)

Once a Phase 01 disconnect-cause.json zip is in hand:

**Decision tree**:

| `keepalive_timeout_fired` | Last `last_recv_ts_secs` on main vs. session uptime | pcap tail | Diagnosis | Branch |
|---|---|---|---|---|
| true | gap of ≥30 s before disconnect | no FIN / RST from server in window | **Server stopped sending PINGs**, or PINGs lost on path. The server's own connectivity timer fires concurrently. | C.1 (proactive client-side PING) |
| true | gap of ≥30 s before disconnect | server PINGs visible in window, ryll PONGs delayed > 30 s | **Client-side starvation.** Most likely macOS App Nap throttling the tokio runtime when ryll is not foreground. | C.2 (disable App Nap on macOS) |
| false | normal traffic up to ~T-1 s | server FIN / RST at T | **Server-side close** — this row should not occur unless something other than the rcc timeout is killing us (e.g. agent disconnect, ticket re-validation on a partial reconnect). | C.3 (investigate the specific server log line) |
| true | last recv was server PING ≤500 ms before disconnect | ryll PONG was queued but never went out | tokio send-side starvation; same as C.2 substantively. | C.2 |

Sub-cases:

- If the disconnect-cause.json's `per_channel.main.ping_recv_count`
  is zero or near-zero across the whole session (not just the
  failure window), the server has not been PINGing at all —
  unusual for QEMU but possible. Confirms C.1.
- If display channel was active (`per_channel.display.bytes_in`
  rising) right up to the disconnect moment but main was idle,
  that's evidence main is being singled out — plausibly App
  Nap doesn't single out one channel, but tokio task scheduling
  can if main's task happens to be suspended on the wrong
  resource. Lean toward C.2.

**Output of Block B**: a one-paragraph summary of the chosen
branch, committed to this plan as a "Diagnosis" section
appended below "Approach" before any C-block code lands.

### Block C — Root-cause fix (one of)

#### C.1 Proactive client-side PING

If diagnosis is "server stopped PINGing", introduce a
client-driven PING on the main channel. Send `SPICE_MSGC_PING`
every **10 s** in the absence of recent server PING activity.
The server (and spice-gtk-style proxies) will respond with
PONG, resetting both sides' liveness timers.

The `Ping` opcode is a symmetric protocol message — the SPICE
spec defines it for both directions (`spice-gtk/src/channel-base.c:43`
treats inbound PING uniformly). The server side at
`/srv/src-reference/spice/spice/server/red-channel-client.cpp`
handles client-sent PINGs in the same connectivity-timer reset
path as any other inbound byte; we won't surprise it.

Site: extend the main-channel select loop at
`main_channel.rs:212-313` with a fourth branch:

```rust
_ = tokio::time::sleep_until(last_send_or_pong_recv + Duration::from_secs(10)) => {
    let ping = build_client_ping();  // SPICE_MSGC_PING
    self.send(ping).await?;
    // last_data_received does NOT reset here — we want to know
    // when the *server* last spoke to us, not us to it.
}
```

`last_send_or_pong_recv` is a new local (not added to the
snapshot — it's transient) tracking either our last send or
the last server PONG, whichever is later. This ensures we
don't flood with PINGs immediately after the user moves the
mouse (which already produces inputs traffic, keeping the
server happy on a different channel — but the server's
connectivity check is per-channel; main needs main-channel
traffic).

Snapshot fields to add on `MainSnapshot`:

```rust
pub client_ping_send_count: u32,
pub last_client_ping_send_ts_secs: Option<f64>,
```

So a future disconnect-cause.json shows whether the proactive
PING was firing as expected.

Cost: one 11-byte message every 10 s = 1.1 byte/s during idle.
Trivially below the noise floor of any other traffic.

#### C.2 Disable App Nap on macOS

If diagnosis is "client-side runtime starvation":

macOS App Nap is the most likely culprit — it activates when
an app is not the active window and not playing audio,
suspending its runloop / GCD queues. tokio sleeps and socket
reads are subject to it. ryll's audio playback is on a
separate channel and may not always be active (no audio in the
guest = no playback channel data = nothing keeping us awake).

Fix: call `NSProcessInfo.beginActivityWithOptions:reason:` on
startup with `NSActivityUserInitiated | NSActivityIdleSystemSleepDisabled`
(or at least `NSActivityUserInitiated | NSActivityLatencyCritical`),
holding the resulting `NSObjectProtocol` for the lifetime of
the SPICE session. This is the documented opt-out from App Nap
and is what apps like Zoom and SSH clients use.

Implementation:

- New crate dep: `objc2` (already in the workspace via egui's
  macOS path) or a small `extern "C"` block. Probably the
  cleanest: a `#[cfg(target_os = "macos")]` module
  `ryll/src/macos.rs` exposing `begin_user_activity()` →
  returns an opaque guard struct that calls
  `endActivity:` on drop.
- Call from `RyllApp::new` after the connection thread has
  spawned.
- Drop the guard when the session ends (Drop on `RyllApp` or
  on the connection-thread cleanup).

This is a no-regret fix even if the actual root cause is
something else, **as long as we are confident App Nap could
cause issues** — but introducing a permanent runloop-pin to
work around an unconfirmed cause is worse than confirming
first. Hence the Block B gate.

Cost: zero additional traffic. Slight increase in idle CPU
when ryll is not the active app (macOS will not throttle
us). This is the tradeoff every interactive remote-display app
makes.

Sub-task: also call `IOPMAssertionCreateWithName` with
`kIOPMAssertionTypePreventUserIdleSystemSleep` if the user has
explicitly requested "don't let the host sleep while connected"
— defer this to a later phase, mention here so we don't tangle
the App Nap fix with a different assertion.

#### C.3 Server-side close investigation

If diagnosis is "server-side close, not rcc timeout": this is
unexpected and invalidates the hypothesis baseline. Stop
implementing and return to triage — likely we have a different
bug than K1. Re-open the master plan.

### Block D — ryll's own 30 s timeout

Independent of the C-block selection, the ryll-side mirror
timeout at `main_channel.rs:297` is currently a footgun: it
fires at the same 30 s as the server, sometimes racing the
server, and we can't tell which closed first from the modal
path. With Block A (auto-reconnect) and Block C (root cause
addressed), the mirror timeout has three options:

(D.a) **Keep at 30 s.** Defensive: if the server somehow
disappears without RST (host hard-killed, network partition),
we still notice in 30 s. With auto-reconnect, the user sees a
brief "reconnecting" flash. This is the conservative option.

(D.b) **Extend to 90 s.** Lets the server's 30 s window fire
unambiguously first when the server is still alive — the pcap
will then show server FIN/RST instead of our local timer
firing, which is more informative for future debugging. Still
catches truly-dead-server cases within 90 s.

(D.c) **Remove entirely.** Rely on TCP keepalive (75 s to
detect a dead peer) plus the channel read returning Err on
RST. Simplifies the code path; downside is in the unlikely
case the kernel TCP keepalive fails to detect death, we hang
forever.

Pick **(D.b)**: keep the timeout but extend to 90 s. Cost is
negligible, debuggability improves materially. Add a one-line
comment at the timeout site explaining why 90 s and not 30 s
("server's own check is 30 s; this is a backstop for when the
server itself is dead or unreachable, not a primary
mechanism").

## Open questions

1. **Should auto-reconnect retry against a fresh ticket?** If
   the deployment supports it, the conductor / gateway
   (Kerbside, oVirt manager) can issue a new ticket on demand.
   ryll has no current path to request one. Phase 02 does not
   add this; the .vv-file ticket is what we have. If the
   .vv-file flow grows a "refresh" hook (e.g. browser
   integration in conductor), revisit.

2. **Should the auto-reconnect attempts share the disconnect
   modal's reason text?** Today the modal shows the original
   error. After auto-reconnect failure, we should show the
   *latest* attempt's error (most informative — the original
   may have been a transient blip while the latest is the real
   failure mode). Yes — track latest error in the
   `ReconnectState::Modal { latest_error }` variant.

3. **Macros / build-time gating for the App Nap fix.** Cargo
   features vs. `#[cfg(target_os = "macos")]`? Use cfg — App
   Nap is platform-conditional behaviour, not a feature
   flag. The non-macOS path returns a no-op guard, keeping the
   call site identical.

4. **Auto-reconnect during initial connect.** Today the link
   establishment at `session.rs` can fail (host unreachable,
   bad cert, bad ticket). Should auto-reconnect cover initial
   failures too? Defer: initial-connect failures are
   user-visible immediately and the user is already
   interactive at that moment. Auto-reconnect adds value when
   the user is *not* in front of the screen.

5. **Telemetry / counters.** Should we expose
   `auto_reconnect_count` somewhere visible (status bar, bug
   report)? Add to the existing channel-state JSON so a future
   bug report shows whether the user's session was rocky. Cheap
   and informative.

## Tasks

### Block A (no-regret, lands without Phase 01 data)

- [ ] Add `ReconnectState` enum on `RyllApp` (`app.rs`),
      replacing the implicit boolean `show_disconnect_dialog`.
      State transitions only via the central event handler and
      the GUI tick.
- [ ] In the GUI tick (`update()` in `app.rs`), poll
      `ReconnectState::Pending` deadlines and trigger
      `reconnect()` when reached.
- [ ] Wire `ChannelEvent::Disconnected` / `Error` handlers to
      transition Idle → Pending(1) — preserving the existing
      Phase 01 disconnect-snapshot call. Do not bypass the 60 s
      cooldown; auto-reconnect attempts that fail will mostly
      hit cooldown after the first.
- [ ] Add status-bar "Reconnecting… (n/3)" widget in the
      bottom panel. Match the existing FPS/connected widget
      style.
- [ ] Push a `NotifySeverity::Warn` notification on each
      attempt failure (source `NotificationSource::BugReport`
      to keep the producer set tidy).
- [ ] Render the three modal variants from §A.6 at
      `app.rs:3119` — `Generic` (Reconnect + Quit),
      `OneShotConsumed` (Quit only), `TicketExpired` (Quit
      only). Track `latest_error` in `Generic` for context.
- [ ] Extend the .vv parser at `ryll/src/config.rs:266` to
      read `delete-this-file` (existing standard key) into a
      new `Config::ticket_is_single_use: bool` field. Plumb
      through to `RyllApp` via `Config::from_args`.
- [ ] Extend the .vv parser to read the new
      `ticket-valid-until=<unix-ts>` extension key into
      `Config::ticket_valid_until: Option<SystemTime>`. Plumb
      through to `RyllApp`. Tolerate missing or malformed
      values (key absent → `None`; malformed → log a `warn!`
      and treat as `None`, do not fail the connect).
- [ ] When `ticket_is_single_use` is true, the auto-reconnect
      state machine refuses to enter `Pending`; disconnect
      goes straight to `Modal { OneShotConsumed }`.
- [ ] When `ticket_valid_until` is set and now() >= it at any
      `Pending` deadline check, transition to
      `Modal { TicketExpired { expired_at } }` instead of
      retrying.
- [ ] Pre-disconnect warning: in the GUI tick, when
      `ticket_valid_until` is set and within 30 s of expiry
      (and notification not yet pushed for this session), push
      a `NotifySeverity::Warn` "Session ticket expires in 30
      seconds." Track a `ticket_expiry_warned: bool` on
      `RyllApp` to fire once.
- [ ] If `ticket_valid_until` is set but in the future at
      disconnect time, render `TicketExpired` anyway and log
      `warn!` "ticket-valid-until in the future at disconnect
      time, possible clock skew" (§A.6 edge case).
- [ ] In `RyllApp::reconnect()`, clear
      `MainSnapshot::keepalive_timeout_fired` (Phase 01 OQ #3
      done here, not in Phase 01).
- [ ] Add `auto_reconnect_count: u32` to the channel-state
      JSON (open question 5). Bump it on every transition into
      Pending.
- [ ] Unit tests:
  - State machine transitions: Idle → Pending(1) → Pending(2)
    → Pending(3) → Modal{Generic} on three failures.
  - Cooldown and auto-reconnect interact correctly: each
    failed attempt within 60 s skips snapshot but continues
    attempting.
  - `delete-this-file=1` path: disconnect → Modal{OneShotConsumed}
    without entering Pending.
  - `ticket-valid-until` past: disconnect at any point →
    Modal{TicketExpired}; Pending deadline check honours
    expiry mid-cluster.
  - `ticket-valid-until` future: warning fires once at T-30 s.
  - .vv parser: round-trips both keys; malformed
    `ticket-valid-until` logs warn and yields `None`.
- [ ] Update README's "console.vv support" section to note
    ryll's interpretation of `delete-this-file=1` (skip auto-
    reconnect) and the new `ticket-valid-until` extension key
    (link to the kerbside-wt-docs extensions doc).
- [ ] Manual integration check (notes only): kill SPICE server
    while connected with a regular .vv, observe three attempts
    then Generic modal. Repeat with `delete-this-file=1`,
    observe immediate OneShotConsumed modal. Repeat with a
    `ticket-valid-until` in the past, observe TicketExpired
    modal.

### Block B (analysis, no code)

- [ ] Reproduce K1 with Phase 01 build and capture at least
      one disconnect-cause.json zip. Document: idle scenario,
      time to disconnect, contents of `disconnect-cause.json`.
- [ ] Walk the decision tree above. Append a "Diagnosis"
      section to this plan with the chosen branch and
      evidence.

### Block C (one of, conditional on Block B)

#### Block C.1 — Proactive client PING

- [ ] Add `SPICE_MSGC_PING` builder in
      `shakenfist-spice-protocol/src/messages` (verify name —
      it should mirror the existing `SPICE_MSG_PING` but
      client→server; if not present yet, add).
- [ ] In `main_channel.rs:212-313` select loop, add fourth
      branch driven by `last_send_or_pong_recv + 10 s`. On
      fire, send a client PING and update the local timestamp.
- [ ] Add `client_ping_send_count` and
      `last_client_ping_send_ts_secs` to `MainSnapshot`
      (`shakenfist-spice-renderer/src/snapshots.rs`); update
      at the send site.
- [ ] Extend `PerChannelDiagnostics` and `DisconnectCause`
      (`ryll/src/bugreport.rs`) to surface the new fields, so
      future disconnect-cause.json shows whether proactive PING
      was firing.
- [ ] Unit test: select loop fires the PING branch when no
      send or PONG within 10 s; does not fire when traffic is
      flowing.

#### Block C.2 — Disable App Nap on macOS

- [ ] Add a `ryll/src/macos.rs` module
      (`#[cfg(target_os = "macos")]`) with
      `begin_user_activity()` → opaque guard via
      `NSProcessInfo.beginActivityWithOptions:reason:`
      (`NSActivityUserInitiated | NSActivityLatencyCritical`).
- [ ] Add a no-op stub for non-macOS targets so the call site
      compiles unconditionally.
- [ ] Hold the guard on `RyllApp` for the lifetime of the
      session. Drop on session end / app close.
- [ ] Choose between `objc2` (workspace already pulls a tree
      of objc bindings via egui's macOS backend — verify and
      reuse) or a small hand-rolled `extern "C"` against
      `Foundation`. Pick whichever requires fewer new deps.
- [ ] Document in README's macOS section: ryll opts out of App
      Nap to keep the SPICE session responsive when not in
      foreground.
- [ ] Manual integration check: with build C.2, leave ryll in
      background overnight on macOS. Verify no disconnect.

#### Block C.3 — Server-side close investigation

- [ ] Stop. Reopen master plan triage; the K1 hypothesis is
      wrong. Capture findings in NOTES.md for the triage
      session.

### Block D (independent, lands with C-block)

- [ ] Extend the client-side keepalive timeout at
      `main_channel.rs:219` from 30 s to 90 s. Add a comment
      explaining the change ("backstop for dead/unreachable
      server, not a primary mechanism — the server's own check
      is at 30 s and the rcc disconnect message is more
      informative than our local timer").
- [ ] Update Phase 01's
      `test_collect_per_channel_round_trips_keepalive_and_traffic`
      assertion if it referenced 30 s anywhere (grep — it
      shouldn't, but verify).

### Wrap-up

- [ ] Update `ARCHITECTURE.md`: new "Auto-reconnect with
      backoff" section describing the state machine and the
      three modal variants. New sub-section under "Auto-snapshot
      on channel disconnect" describing the C.1 proactive PING
      (if applied) or C.2 App Nap opt-out (if applied). Note
      `delete-this-file` interpretation and the new
      `ticket-valid-until` extension key with a link to the
      companion doc.
- [ ] Update `AGENTS.md` with the new `ReconnectState`
      pattern (§20-style entry).
- [ ] Update `PLAN-session-001-feedback.md` Execution table
      status for Phase 02 → Done.

## Companion docs

This phase adds the first ryll-defined console.vv extension
key (`ticket-valid-until`) and ascribes a non-spec interpretation
to a standard key (`delete-this-file=1` → skip auto-reconnect).
Both must be discoverable to producers who want their .vv
files to drive ryll's behaviour correctly.

A new doc lives in the **kerbside-wt-docs** worktree at
`/home/mikal/src/shakenfist/kerbside-wt-docs/docs/spice/console-vv-extensions.md`
(committed alongside the existing protocol docs `channel-protocols.md`
and `spice-link-protocol.md`). The doc covers:

- A short preamble explaining what console.vv is and why ryll
  documents extensions separately (the standard format has no
  registry, and ryll consumes some standard keys with stronger
  semantics than the spec requires).
- A "ryll's interpretation of standard keys" section documenting
  `delete-this-file=1` as a one-shot ticket signal (rationale
  + implication: ryll skips auto-reconnect).
- An "Extensions" section documenting `ticket-valid-until=<unix-ts>`
  with format, semantics, and ryll's behaviour when set / unset.
- A "How to support these in your producer" section with sample
  console.vv content that Kerbside / oVirt operators can paste.
- A "Future extensions under consideration" section so this
  doc is the obvious place to discuss new keys.

Filing RFEs against producers (Kerbside, oVirt) once the doc
exists is part of this phase's wrap-up but not blocking —
ryll's day-one behaviour without producer changes is correct
because absent keys are no-ops.

## Out of scope

- Reconnect with a fresh ticket. Requires conductor /
  gateway-side support not currently available; see open
  question 1.
- Surfacing non-critical channel disconnects (cursor /
  playback / usbredir / webdav) to the user beyond the existing
  Phase 01 snapshot. That is **Phase 09** (F1 — connection
  events in the notifications pane).
- Per-channel auto-reconnect — once a channel drops mid-session
  under one-shot tickets, it cannot be re-linked, so per-
  channel retry is wasted effort. Whole-session reconnect (this
  phase) is the only meaningful retry granularity.
- Implementing the wider standard-virt-viewer-keys parity gap
  (`title`, `fullscreen`, `disable-channels`, `secure-channels`,
  `enable-usbredir`, `proxy`, etc. — see `config.rs:266`).
  ryll's .vv parser today reads only host/port/tls-port/password/
  ca/host-subject. That gap deserves its own master plan with
  the standard-key compat as the framing; tangling it into K1
  conflates two unrelated motivations (reconnect correctness
  vs. .vv compat). This phase adds only the two keys it needs.
- Producer-side changes (Kerbside / oVirt emitting
  `ticket-valid-until`). Tracked as RFEs after the
  console-vv-extensions.md doc lands, not implemented here.
- Changes to the channel teardown semantics (Disconnected
  event → loop break). The signal flow is fine; only the
  disconnect *response* changes.
- Telemetry beyond the channel-state JSON's
  `auto_reconnect_count`. A persistent metrics store is its
  own master plan if we ever need it.
- macOS Idle Sleep prevention (`IOPMAssertion…`). Different
  problem, different opt-in, different lifecycle. Mentioned in
  C.2 only to clarify it is *not* what App Nap opt-out covers.
- Linux / Windows equivalents to App Nap. Linux has no
  equivalent; Windows has connected-standby restrictions but
  ryll has not been observed to hit them. Revisit only if
  reproduced.
