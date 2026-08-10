# Wire Monitoring — seeing what the services say to each other

Status: draft · Date: 2026-08-10 · Owner: pierre

Companion to [`architecture.md`](architecture.md), which owns the IPC contract (§2) and the
observability contract (§8). This document covers one question those two leave open: how
anybody sees the messages the services actually exchange — at the bench while building, and
on a robot in someone's home when support is asked what went wrong.

Nothing here is implemented. This is the design to argue with before any of it is written.

## 1. Two questions, and why they are one mechanism

"Monitor the message exchange" is two different requests wearing one name.

| | **the trace** | **the meter** |
|---|---|---|
| Asked by | a developer at the bench | support, or a script, on a live robot |
| Question | "the phone pressed update and nothing happened — where did it stop?" | "is this robot healthy, and who is talking to it?" |
| Content | individual messages, in order, with cause | counts, rates, latencies, drops |
| Span | one operation across several services | one service over hours |
| Volume | everything, briefly | a fixed-size summary, always |
| Lifetime | while you watch, or in the journal afterwards | continuous, bounded memory |

They are not two systems. They are **one event stream folded two ways**: the trace prints
each event, the meter accumulates them. Building them separately is how a robot ends up with
a log saying one thing and a counter saying another, and no way to tell which lied.

`robotctl monitor` already establishes the shape in this codebase — one stream from
`robot.state`, rendered as a repainting frame for a terminal and as one line per tick for a
pipe. Same stream, two renderings, chosen by context. This extends that idea from one
service's telemetry to every service's traffic.

## 2. What is observable today

The message graph, with `mediad` still to come:

```
robotctl ──┐
padd ──────┼──▶ robotd    robot.*, incl. ~50 Hz intent notifications
btd ───────┼──▶ configd   net.*, system.*
           └──▶ updaterd  update.*
updaterd ──────▶ robotd    safeToRestart, health, modelApi, remoteSessionActive
```

What exists:

- **Mutating calls are logged with the caller.** `configd` and `updaterd` both authorise
  against `SO_PEERCRED` and log method + uid/gid (+ pid, in `updaterd`) at `info`, refusals at
  `warn`. This is the seed of the whole design: it is already the right event, with the right
  identity attached, at the right place — it just covers one call class in two of the four
  services.
- **`robotctl monitor`** renders `robotd`'s state stream, which is the control loop's
  telemetry rather than its traffic.
- **Drop counters exist but are invisible.** `robotd` and `updaterd` both notice a subscriber
  falling behind and log it at `debug`; nothing accumulates it, so "this robot has been
  dropping a third of its state frames all week" is not a question anyone can ask.

What is missing:

- **`robotd` observes nothing.** It has no peer policy and logs no calls, so the busiest
  socket on the robot is the one with the least visibility.
- **Nothing is correlated.** JSON-RPC ids are per-connection. `btd` forwards lines verbatim so
  an id survives that hop, but `updaterd`→`robotd` mints its own starting at `1`. There is no
  way to join a phone's tap to the `robot.safeToRestart` it eventually caused.
- **Read-only calls are entirely dark**, deliberately ungated (§2.2) and therefore unlogged.
  "Support cannot change this robot but must be able to inspect it" is the right rule; it
  also means the inspection traffic is the traffic nobody records.

## 3. Where to observe: the dispatch boundary, not the socket

The tap goes where a service turns a line into a typed `Call` and produces a `Response` —
**not** on the socket, and not in a process between the two.

### 3.1 Why not an interposing proxy

The cheap design is a `robotctl tap` that binds a socket, splices to the real one, and prints
both directions. Zero daemon changes, and every client already takes a socket override
(`robotctl --socket/--robot-socket/--config-socket`, `btd --updater-socket/…`, `padd
--socket`), so it would work today. Two properties disqualify it as the durable answer:

- **It launders authorisation.** `may_mutate` reads `SO_PEERCRED`, which behind a proxy
  reports the *proxy's* uid for every caller. So either mutating calls stop working through
  the tap, or — run as an allowed uid to make them work — the tap becomes a tool that grants
  any peer permission to change the robot. That is not a thing to leave on a board.
- **It is blind to every transport but one.** §4.1 is "one definition, many transports": BLE,
  unix socket, WebSocket, WebRTC datachannel. A unix-socket proxy can see one of the four. It
  cannot see the phone→`btd` leg, and it will not see `mediad`'s remote gateway. A tap at
  dispatch sees all of them by construction, because that is where every transport converges
  on `Call`.

It is also blind in a subtler way: bytes on a wire do not say that a call was *refused by
policy*, that a velocity was *clamped by safety*, or that a state frame was *dropped because
a subscriber lagged*. Those are the events worth having, and none of them exist on the wire.

### 3.2 Alternatives considered

| Option | Why not |
|---|---|
| Interposing proxy (`socat`, or ours) | §3.1: launders `SO_PEERCRED`, sees one transport, cannot see internal outcomes |
| `strace -e trace=read,write -p` | No code, genuinely useful once. Heavy on a Pi, no decoding, no redaction. Keep as a documented escape hatch, not a design |
| `bpftrace` / eBPF uprobes | Same, plus a toolchain a shipped board does not have |
| A message broker everything routes through | Fights invariant 1 outright: another component that can fail, on the recovery path (§2.2) |
| Each service writes its own wire log file | Fights §8.1: one mechanism, one retention policy, one place to look |

## 4. The tap

Four events, emitted by a service about its own traffic:

| Event | Carries |
|---|---|
| `request` | peer identity, transport, method, correlation id, params (§5), arrival time |
| `response` | correlation id, ok/error + code, dispatch duration |
| `notify` | method, subscriber, correlation id when caused by a request |
| `drop` | what was dropped and why — subscriber lagged, queue full, line too large, parse error |

`drop` is not an afterthought. It is the only event that explains a robot that looks fine from
inside and wrong from outside, and it is the one a socket-level observer can never produce.

### 4.1 Where the code lives

`duck-ipc-proto` gains an observation module; each serve loop calls it at those four points.
That is roughly four lines per service and no restructuring.

**Not, initially, a shared serve loop.** The accept/read-line/parse/dispatch/write loop exists
in three near-identical copies (`updater/src/ipc.rs`, `configd/src/main.rs`,
`robotd/src/main.rs`) and extracting it is a real and probably correct refactor — but the
copies differ in peer policy, line-length handling and subscription fan-out, and binding the
monitor to that refactor makes both bigger and riskier than either is alone.

Sequenced this way the refactor stays a decision on its own merits: if the loop is later
extracted, the tap moves inside it unchanged, and the four call sites collapse to one. If it
is not, the monitor still exists. Nothing is rewritten either way.

The cost of the light version, stated plainly: **a new service can forget to call the tap.**
A shared loop would make that unrepresentable, the way `route.rs`'s exhaustive match makes a
forgotten BLE route unrepresentable. Until the loop is shared, this is a review rule rather
than a compiler rule, which is weaker and should be written down as such.

## 5. Params are opt-in, and redacted even then

`NetConnectParams` and `AuthenticateParams`/`SetPairingPinParams` carry hand-written `Debug`
impls that redact the wifi PSK and the pairing PIN, for a stated reason: services log calls
they could not handle, and a customer's wifi password must not be recoverable by anyone who
can run `journalctl`.

A monitor is exactly the thing that undoes that. It sees the JSON line *before* any `Debug`
impl runs, and if it writes to journald it undoes it durably. So:

- **The default rendering is method + peer + outcome + timing.** No params. This is enough for
  almost every trace: which call, from whom, what came back, how long.
- **Params are a deliberate opt-in**, per invocation, never the default and never the shipped
  configuration.
- **Even opted in, rendering goes through the typed `Call`**, so the existing redacting `Debug`
  impls apply. A tap that formats the raw line is a tap that routes around the redaction — the
  raw line must not be the thing that gets printed.

This is the strongest single argument for tapping at dispatch rather than at the socket: at
dispatch the redaction is the natural path, and at the socket it is a thing to remember.

## 6. Correlation

Neither rendering answers "where did it stop?" without a way to join events across processes.

`Request` gains an optional correlation id, and three rules govern it:

1. **A transport adapter mints one if absent.** `btd` today; `mediad`'s gateway later. A call
   arriving from outside the robot gets its identity at the front door.
2. **A service propagates it into any call it makes as a consequence.** `updaterd`→`robotd`
   is the case that exists now, and the one that is currently unjoinable.
3. **Notifications caused by a request carry it** — which is what makes an update's progress
   stream attach to the tap that started it rather than floating free.

Format: an opaque string, bounded length. Locally `<service>-<pid>-<counter>` is free,
unique on one box, and readable in a log; a remote peer has no pid, so the *rule* is strict
and the *format* is advisory. Nothing may parse it.

This is a wire change and needs a `PROTOCOL_VERSION` bump. Doing it now is cheap and doing it
later is not — it is the same category as `min_supported` and `schema_version` in
`architecture.md` §10, fields that went in before they were used because they cannot be
retrofitted. The install consequence is real and worth naming: a bumped version means a stale
`robotctl` or a mismatched `btd` is refused at `hello`, so board and laptop have to move
together.

## 7. The trace

One decoded line per event, correlated, with peer identity — the bench rendering.

```
12:04:31.201  btd      → updaterd  update.apply      trace=btd-812-7   peer=uid:0 pid=812
12:04:31.203  updaterd → robotd    robot.safeToRestart  trace=btd-812-7
12:04:31.204  robotd   → updaterd  ok safe=true         trace=btd-812-7   1.2ms
12:04:31.240  updaterd ⇢ btd       progress phase=download 12%  trace=btd-812-7
```

Grouping by correlation id turns that into the tree a developer actually wants, and the
grouping is the whole reason §6 exists.

Two things this must respect:

- **Off by default**, and enabled without a rebuild — a level on a dedicated `tracing` target,
  so `RUST_LOG` turns it on per service on a board that is already misbehaving.
- **Rate-bearing traffic is separated from the rest.** `padd` notifies at ~50 Hz and
  `robot.state` pushes up to 50 Hz per subscriber; a trace that treats those like an
  `update.apply` is unreadable, and one that writes them to the journal at anything above
  `trace` evicts the logs an incident needs (§8.1's 86k-entries-a-day argument, which applies
  here more strongly than it did there).

## 8. The meter

A fixed-size summary per service, always on, readable over IPC — the field rendering.

Candidate content, all of it foldable from §4's four events:

- calls by method, and errors by code;
- notifications emitted and **dropped**, per stream;
- dispatch duration, p50/p99;
- connections currently open, with peer identity and age;
- when each of those was last reset.

Bounded memory is a requirement, not an aspiration: this runs forever on a robot nobody
reboots. Fixed method set, fixed code set, no unbounded keys — a per-peer map keyed by pid
grows without limit on a board where a client reconnects in a loop.

**It lives in each service, and `robotctl` fans out.** No aggregator: invariant 1 says the
recovery path cannot depend on a component, and a stats collector would be one. The
consequence is that `robotctl` renders a partial picture when a service is down — which is
correct, and is itself the most important line in the output.

**Distinct from `robot.health`.** Health answers "is this robot fit to run"; the meter answers
"is this robot's IPC working, and who is using it". `LoopHealth` already reports tick rate and
must not be duplicated here.

## 9. How the events get out

Two candidate transports, and they are additive rather than alternatives:

- **`tracing` on a dedicated target, first.** Journald already solves persistence, retention
  and offline reading; `RUST_LOG` already solves per-service enablement; no new socket, no new
  authorisation surface. This covers the whole trace use case on day one.
- **A `wire.subscribe` notification stream, later.** What a live TUI wants, and structurally
  the same thing `update.subscribe` and `robot.subscribe` already do. Deferred because it
  needs an authorisation decision of its own — a stream of *everything anyone sends* is
  strictly more sensitive than any single call on it, and it cannot inherit the "read-only
  calls are ungated" rule — and because it must not observe itself into a loop.

Both consume the same internal event, so the second is additive and neither blocks the other.

## 10. What this must never do

Stated as invariants because a monitor is exactly the kind of component that acquires them by
accident:

1. **Nothing may depend on it.** Disabled, absent, or broken, every service behaves
   identically. It is not on the recovery path and must never become a reason a robot cannot
   be recovered (§1.1, invariant 1).
2. **It must not block the control loop.** No allocation-heavy formatting and no I/O on
   `robotd`'s tick path; the disabled tap is a branch not taken, and the enabled one hands off
   (§1.1, invariant 3).
3. **It must not become a credential leak.** §5.
4. **It must not widen access.** Reading the meter is a read; nothing about monitoring
   justifies a new way to change the robot.

## 11. Open questions

1. **Is a shared serve loop the right next refactor?** §4.1 sequences the monitor so it does
   not depend on the answer, but the answer changes whether "a new service forgets the tap" is
   a review rule or a compile error.
2. **Correlation id on responses and notifications too, or only requests?** Requests alone are
   enough to build the tree if each service echoes it; echoing costs a field on every reply.
3. **Which events, if any, earn `info` permanently?** Today's "mutating request authorised /
   refused" pair clearly does. Connection open/close with peer identity probably does. The
   per-message stream clearly does not. The line wants drawing once rather than per service.
4. **Does the meter survive a restart?** Counters that reset on every `robotd` restart answer a
   different question from ones that persist, and the update system restarts services on
   purpose. If it should persist, it is `/var/lib` state (§8.2) and needs an owner.
5. **What does `mediad` change?** It is the first service that is both a client and a gateway,
   and the first to carry the API over transports with no `SO_PEERCRED` equivalent. Its
   arrival is the moment §6's rule 1 stops being theoretical.

## 12. Build order

Each step is useful alone, and none of them requires the next:

1. **The correlation id and the `PROTOCOL_VERSION` bump.** First because it is the only wire
   change here, and the cost of it grows with every client that exists.
2. **The tap and its four events, plus the `tracing` sink.** At this point the trace works and
   `robotd` stops being dark.
3. **The meter**, folded from the same events, and `robotctl` fanning out to render it.
4. **`wire.subscribe` and a live view** — only once something wants it, and with the
   authorisation question of §9 answered rather than assumed.
