# The autonomous behavior stack — what it inherits, and ideas waiting for it

The brain is the biggest untracked gap in the [parity audit] (§03): the runtime's
`autonomous.rs` exists nowhere in the daemon and no design doc owns it yet. This file is the
holding pen — what the port has to cover, and the ideas from the theremin/chorale work
(2026-08) that should land as *behaviors in that stack* rather than as more ad-hoc modes.

[parity audit]: https://claude.ai/code/artifact/4ea45b32-a298-42ea-bcb4-1d2a7a567948

## To port from the runtime (audit §03)

A 16-state machine — Chill, LookAround, Wander, TurnInPlace, Zoomies, Startle, Stretch,
Ruffle, Preen, Sneeze, Dance, GroundPick, Nap, BallPlay, Petted, Held — built on:

- an **energy/mood model** driving state choice
- **novelty-grid exploration memory** for Wander
- **ToF obstacle avoidance** with freshness gating
- contrast-based **startle**; **sound reactions** (noise vs voice) with self-audio /
  self-motion gating
- **ball play** (approach / line up / kick), a **nap cycle**, **petting reactions**
- the in-process gamepad↔autonomous toggle (DpadRight 2 s), `--autonomous-max-speed`

Its inputs all exist in the daemon now: ambient sound events (step 2 — logged, no consumer),
depth frames (step 3), classified trunk-frame obstacle points (step 5), and the voice tags
(step 2, "ready and waiting for the brain step"). `Held` depended on pickup detection, which
is deprecated — decide whether to revive it or drop the state.

## New inputs the brain gets for free (2026-08 work)

Things the theremin/chorale steps built that the runtime's brain never had:

- **Nearby ducks, by stable id** — the chorale beacon (`ChoraleBeacon`), which survives BLE
  address rotation (see `docs/../memory`: never key on the address)
- **A shared beat** with no clock sync (`sounds::chorale::beat`) — ±20 ms across ducks
- **RSSI per advertisement** — free coarse distance (near / far / approaching)
- **~245 spare bytes** of extended-advertising payload
- **A live synth voice** (`sounds::Stream`): pitch/level/vowel at runtime, not just bank wavs
- **Hand distance from the ToF** (`kinematics::hand::Tracker`)

## Behavior ideas, roughly by charm-per-line

**Social (BLE presence — the new territory):**

- **Recognition & greeting** — keep a persisted list of duck ids met; `greet` a stranger,
  a warmer sound for a friend, a `peck`/sigh when a friend's beacon goes stale. Friendship
  as a met-count that changes the greeting over time. Reads as *memory*; needs no sync.
- **Lonely / content** — a duck alone calls out occasionally; a duck with company doesn't.
  An input to the mood model, not a state.
- **Excitement on approach** — RSSI rising for a known id → visible anticipation.
- **Marco Polo** — one duck hidden, another guides you by quacking faster as RSSI grows.
- **Follow-the-leader** — RSSI holds spacing, ToF handles the duck directly ahead.
- **Applause / social feedback** — one duck does a roulade, nearby ducks react.
- **Telephone** — a message hops duck to duck through the spare payload, mutating.
- **Voting** — preferences in the beacon payload; majority picks the group's next behavior.

**Beat-synced motion (cash in the sync work where it shows — no speaker involved):**

- **Group head-bob / sway** — all ducks in phase on the shared beat; visual sync tolerates
  ~50 ms where audio wanted 20. Group pose on the downbeat of a bar. Conga line.
- **Dance** (already a runtime state) becomes *synchronized* dance when company is present.

**Musical, beyond the chorale:**

- **Call and response** — antiphonal phrases; the gap between phrases hides sync error, so
  it is *easier* than the chorale and more duck-like.
- **A round** — same melody, deliberate multi-bar offsets; the offsets absorb the error.
- **One role each** — drone + rhythmic peck + melody. Small speakers do texture better
  than harmony.

## Duck detector (camera + NPU) — wanted, waiting on mediad

A tiny single-class detector for *our own duck*: precise **bearing** for gaze and following,
which neither ToF nor BLE can give. The RK3566 has a 0.8 TOPS INT8 NPU (`rknpu2` /
`rknn-toolkit2`); a YOLOv8n/11n-class model at 320 input should run ~20–40 ms → 15–30 Hz,
leaving the CPUs alone. Range math: IMX219 ~62° HFOV, a 25 cm duck is ~25 px at 3 m — room
scale, which is the interaction envelope.

- **Gate first:** is the NPU driver on the board? (`dmesg | grep -i rknpu`) — vendor-kernel
  `rknpu2`, not mainline. And `/dev/rga` for free NV12 resize. Ask mediad to tee raw NV12
  from the ISP mainpath; the detector must not decode the streaming MJPEG.
- **Data is the project, not the model.** Duck's-eye-view footage (robot height, robot
  camera) auto-labeled by a big open-vocab model, distilled into the tiny one; synthetic
  renders from the Open Duck CAD for the tail; hard negatives (rubber ducks, white prints).
- **Fusion:** vision cannot tell identical ducks apart — camera = direction, ToF = distance,
  BLE beacon = identity + presence. Follow-the-leader uses all three; "look at each other
  when doing stuff" = beacon says *when*, detector says *where*, `robot.look` does the rest.
- Same architecture as `tofd`/`pet-detect`: a perception worker outside `robotd`, safe to
  kill. Fix the placeholder IMX219 intrinsics during the mediad port (audit §04 TODO).

## Shape notes for the port

- Presence, mood, and the shared beat are **inputs to one brain**, not modes beside it —
  the chorale/theremin grew as explicit modes because there was no brain to hang them on;
  fold them in as states/inputs when it lands ("Petted" is to pet-detect what "Sing" is to
  a heard beacon).
- **The chorale ends up as a spontaneous event, not a command.** `robotctl chorale` is
  bench scaffolding: once ducks run autonomously, a group of them together should
  *sometimes* break into song on their own — a low random chance gated on company being
  present (and plausibly on mood/energy), the way Zoomies or Dance fire, not something a
  user starts. Rare on purpose: a surprise duet is a delight, a jukebox is not. The
  mechanism barely changes — an idle-beacon duck already knows who is nearby, so "decide to
  sing" is one more transition; `[chorale] accept` stays as the consent gate for whether a
  duck may ever join in.
- The chorale's consent rule generalizes: **anything social is opt-in and off = invisible**
  (`[chorale] accept` today; probably one `[social]` switch tomorrow).
- Recognition/greeting is the right *first* behavior: highest charm per line, no sync, and
  it exercises the BLE discovery layer — the part still under hardware suspicion.

## What changed since this file was written (added 2026-08-27)

Four things below the ideas above are now real rather than wanted, and two of them are inputs
nobody planned for:

- **The detector runs on the NPU.** `Detection::bearing()` gives the direction of another duck to
  a fraction of the 62° frame, at 2 Hz for ~79 ms a look. Its box *height* is an unexploited range
  estimate — a 25 cm duck is ~25 px at 3 m — and needs only the IMX219 intrinsics the audit §04
  already owes.
- **The camera is a light meter, for free.** `mediad::exposure` computes the frame's mean luma
  twice a second because auto-exposure needs it, and publishes nothing. It is an *ambient light
  sensor nobody had to add*, and its saturation flag (`at_ceiling`) is a second bit: "this room is
  darker than I can see in".
- **`fallen`** exists on `robotd`'s state and currently gates nothing (limp-fall, 2026-08-24).
- **Ranked election** — `chorale-election` gave the flock a total order by beacon id, and a
  protocol for deferring to it. That is reusable for anything needing "who goes first".

## More ideas, after those (2026-08-27)

### The light meter wants to be nine light meters

The AE walk already samples every 8th pixel of every frame. Bucketing those samples into a 3x3
grid instead of one accumulator costs nothing measurable and turns a brightness scalar into a
**direction of light**. That single change buys:

- **Phototaxis.** The duck turns toward a window, or follows a torch beam across the floor — the
  behaviour that reads most like an animal for the least code, and the one thing a whole-frame
  mean cannot do (measured on a robot: a torch pointed straight into the lens moved the mean by
  almost nothing, because a bright spot is a small part of a dark frame).
- **Bedtime and sunrise.** Lights out is a large, sustained fall in every cell — a duck settles,
  and having settled, says so in its beacon, so the last one still pottering about gets the hint.
  Lights on is a stretch and a greeting to whoever is present. No clock, no configuration, and it
  happens at the right time in a room on the other side of the world.
- **Dazzled.** Mean far above target while the shutter is already at its floor is somebody
  shining a light at the duck. A flinch, a squint, a turn away.

**Cheapest new sense in the codebase**, and it belongs to `mediad` rather than the brain: publish
the grid beside the detections and let the behaviours subscribe.

### Mutual gaze, as the primitive the social stack is missing

A duck seeing another duck is one-sided and unreliable. A duck *knowing it is being looked at* is
the thing that makes a pair of them feel alive — and it is a handshake, not a perception problem:
put "I can see a duck, bearing θ" in the beacon payload. Two ducks whose bearings roughly oppose
have **confirmed** eye contact, and neither had to recognise the other visually.

Everything social gets better with it, because it replaces "do a thing near each other" with "do a
thing *at* each other":

- **A look, acknowledged.** Both bob once, on the next shared beat. Two lines of behaviour, and the
  most legible possible demonstration that the beacon, the detector and the beat all work.
- **Staring contest.** Hold it; first to look away loses; the winner does a small victory ruffle.
- **Checking on you.** A leader glances back mid-follow to confirm the follower is still there,
  and reacts when it is not.

### Fallen-duck rescue

`fallen` gates nothing today. Put it in the beacon and it becomes the flock's most affecting
behaviour: a duck goes down, the others *notice from across the room*, come over — RSSI to close
the distance, the detector to find the bearing for the last stretch — and gather round it making a
fuss until it is up.

Every part exists. It is the clearest case of an existing flag being worth more shared than local.

### Tag

The one game a spectator understands with no explanation. The "it" duck chases the nearest
detected duck; contact is the ToF close *and* the detector's box large; the tag passes over BLE and
the new "it" has to wait a beat before chasing back. Uses identity, bearing, range and the beat at
once, and fails gracefully — a lost detection is just a duck that has to look around again.

### Hide and seek, on the map

`maploc` gives places, which the beacon and the camera cannot. The hider picks a spot far from the
seeker on the shared map, goes there, and goes quiet — no beacon, which is itself the game. The
seeker gets warmer and colder from RSSI alone, and confirms with the detector at the end. The
hider's kidnap watchdog even handles the case where somebody picks it up and moves it mid-game.

### Emotional contagion

One duck startles. Its neighbours startle a beat later, theirs a beat after that, and the ripple
crosses the room at a visible speed because the shared beat gives the delay a rhythm rather than a
latency. Cheap, and it makes a group feel like a group rather than several robots.

The same shape carries the good moods: one duck's zoomies are contagious to a duck that can see it.

### Roll call

Occasionally one duck quacks and the others answer *in beacon-id order* — the total order
`chorale-election` already computes. It sounds like counting, it demonstrates that the flock agrees
on who is present, and it is the debugging tool that is also a delight: a duck that has quietly
dropped off the roster is a gap you can hear.

### Co-localisation on encounter (the ambitious one)

Two ducks that see each other know a relative bearing, an approximate range, and — over BLE — each
other's identity and odometry. That is enough to relate their two maps. **Meeting another duck
becomes a loop closure**: the flock builds one map between them without any of them ever covering
the whole room.

Speculative, and the one idea here that is a project rather than a behaviour. Noted because the
inputs it needs are the same three every idea above uses, so it costs nothing to keep in view.

## If only three of these get built

**The 3x3 luma grid**, because it is a new sense for a few lines inside a loop that already exists,
and phototaxis and bedtime both fall out of it. **Mutual gaze**, because it is the primitive the
rest of the social ideas are written in terms of. **Fallen-duck rescue**, because the flag is
already there and the behaviour is the one that would make somebody who does not care about robots
care about these ones.
