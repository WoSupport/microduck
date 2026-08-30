# Simulation: the same daemons, a body in MuJoCo

**Status:** `robotd --sim` and `duck_control::sim` exist. Everything else here is designed and
measured but not built.

The goal is a duck you develop against exactly as you develop against a robot: the same binaries,
the same units, the same `robotctl`, the same `duckctl open` — with the body in MuJoCo instead of on
the desk. Not a mock, and not a test harness. A twin, with a written-down boundary.

## 1. What using it looks like

One command to get a duck. After that it is a robot, and you already know the commands.

```
duck-sim up                    # one duck in the apartment, ready in seconds
duck-sim up 4                  # four, sharing one MuJoCo window
duck-sim shell duck-a          # you are on duck-a
duck-sim down
```

Inside, nothing is special:

```
duck-a # robotctl health
duck-a # robotctl configure
duck-a # robotctl chorale
duck-a # journalctl -u robotd -f
```

And from your own shell, exactly as with a robot on the desk:

```
duckctl open duck-a
scripts/dev-push.sh microduck@duck-a
```

Four rules the harness holds itself to, because a development tool that needs a runbook does not get
used:

**No setup step anybody has to remember.** `duck-sim up` on a machine that has never run it builds
the rootfs, fetches what is missing and says so — once. Whatever it genuinely cannot do for you, it
prints as the exact line to paste, in the style the rest of `scripts/` already uses.

**Defaults are the common case.** No count means one duck. No scene means the apartment. No
`--cameras` means no cameras, because most sessions do not need them and they are what costs.

**Nothing you cannot stop.** Every duck runs as a systemd unit, so `duck-sim down` is a
`systemctl stop` — not a key combination. That rule was bought with a container that had to be
killed from a second terminal because `Ctrl-]` is `AltGr + )` on a French keyboard.

**A duck is addressable by name.** `duck-a`, `duck-b`, and each gets its own machine-id — so
`robotctl quack` sounds different on each one, since a duck's voice is generated from its serial.
Four ducks in a chorale should not be four copies of one voice.

## 2. Where the seam is

`duck_control::io::RobotIo` — six methods, and the only place a simulator is allowed to exist:

```rust
fn read(&mut self) -> Result<Sensors>;          // joints and IMU, one transaction
fn write(&mut self, targets: &JointTargets) -> Result<()>;
fn set_gain(&mut self, kp: u16) -> Result<()>;
fn set_torque(&mut self, on: bool) -> Result<()>;
fn slow_sensors(&mut self) -> Result<SlowSensors>;   // volts, per-joint temperature
```

Above it, nothing changes: the 50 Hz loop, the ONNX policies, `Safety`, fall detection, odometry,
kinematics, maploc, every IPC call, all of `robotctl` and `duckctl`. Below it there is one thing —
`DynamixelIo` — and the IMU is not separate from it, because on this robot the IMU is a Dynamixel
node read in the same `sync_read` as the fifteen servos.

`FakeIo` was already a full implementation of this trait, which is why `cargo test` needs no
hardware. `RemoteIo` is the third.

**Where a sensor's daemon *is* its driver, replace neither.** `tofd --fake` already synthesises
frames at the loop level, and `tof/src/sensor.rs` says in as many words that the off-board `Sensor`
"is not a fake sensor and must never become one". Simulated depth feeds that existing loop. The same
reasoning will apply to anything else whose driver cannot be separated from its hardware.

## 3. The body protocol

TCP, newline-delimited JSON, one request and one answer per call. `duck_control::sim` is the
implementation and carries the reasoning; the short version:

* **TCP** because a unix path is capped at `SUN_LEN` (~108 bytes), and because the simulator must be
  reachable from outside whatever the daemons run in — a container on Linux, a Linux VM with MuJoCo
  on the host on macOS.
* **JSON** because a tick is ~1 KB, so 50 KB/s, against being able to read a frame with `nc` and
  write the other end in twenty lines of Python. A packed struct shared across two repositories in
  two languages is how this project has lost days before.
* **`TCP_NODELAY`**, not as a micro-optimisation: Nagle delays a small write up to ~40 ms, twice the
  tick, and it would present as a slow simulator.

Requests are `{"op":"hello"|"read"|"write"|"gain"|"torque"|"slow", …}`. The handshake carries
`protocol` (currently 1) and is checked in both directions, because the two halves live in two
repositories and "your simulator is old" and "your daemon is old" are otherwise the same symptom.

**The simulator reports in the robot's own units** — radians, rad/s, mA, and the IMU already
resolved into the trunk frame. MuJoCo knows its own model's ordering, scaling and frames; a
translation layer on this side would be a second place for that to live and drift.

**A dead simulator is one bad tick.** MuJoCo compiles its model, so changing the number of ducks
restarts it, and the ducks must live through that. A broken connection is an error returned to the
caller and a reconnect on the next call — no backoff thread, because the control loop is the retry
timer.

## 4. Faking the radio costs nothing

Presence is already an IPC contract on `robotd`'s own socket: `chorale.subscribe`,
`chorale.beacon` (what to advertise) and `chorale.heard` (what was heard, with an *age* rather than
a timestamp). And `btd` is a **client** of robotd, not a server — so a simulated ether impersonates
nothing and steals no socket path. One process holds one connection per duck, collects what each
wants to advertise, and delivers it to the others with an RSSI derived from the distance between
their bodies.

No change to `robotd`, no protocol work, and three things better than the radio for development:
the age-based synchronisation path is exercised for real; RSSI from ground truth makes range cutoffs
and asymmetric links a knob rather than a staging problem; and `from` is documented as an identity
for de-duplication only, so rotating it on a timer turns the address-rotation bug that cost a day
into a regression test.

## 5. Architecture follows the host

Every artifact this project builds is aarch64. The attractive idea was to run the board's own
binaries on an x86 laptop under `qemu-user` — same bytes, perfect provenance. It was measured, twice,
and the two results point opposite ways.

**The daemon alone is fine.** CI's real aarch64 artifact, emulated on an x86 laptop:

| | |
|---|---|
| `robotctl health` | `50.0 of 50.0 Hz · 3804 ticks · 0 missed` |
| host CPU | 4.7% of one core, nine ONNX sessions loaded, policy driving |
| policy inference | 0.029 ms against a 20 ms tick (measured natively) |

Throughput was never the risk — and the slow part of a real tick, the Dynamixel sync-read, is a
local socket here.

**Under systemd it is not.** Booted in `systemd-nspawn`, aarch64 systemd 257 comes up in 8.7 s and
then cannot start anything:

```
robotd.service:            (code=exited, status=226/NAMESPACE)
systemd-journald.service:  (code=exited, status=243/CREDENTIALS)
systemd-logind, systemd-tmpfiles, console-getty: the same
```

qemu-user 8.2 does not translate the new mount API (`fsopen`, `move_mount`, `open_tree`) that
systemd 257 uses for per-unit namespaces and credentials. Per-unit hardening is the entire reason to
boot a container rather than run seven processes in a terminal, so losing it loses the point. Newer
qemu may fix it; in Debian 13 and Ubuntu 25.04 the static packages are transitional and the real
binaries are dynamically linked, so it is a build-from-source question rather than an apt line.

So the twin runs **the host's architecture**:

* **x86 host** — an amd64 container and a native build. Real units, real hardening, real journal;
  everything except CI's exact bytes.
* **arm64 host (Apple Silicon)** — an arm64 container running the robot's own signed artifact,
  natively. Full provenance, no emulation, identical commands.

The consequence for the update path is small and worth stating plainly: `robotctl update apply` runs
end to end — preflight, signature, artifact hash, compatibility, health gate, auto-rollback — because
that is how `dev-push.sh` installs. What an x86 twin cannot do is install a *published* release.
`board-test.sh` already covers the real artifact on the real architecture in CI.

## 6. What it is and is not a twin of

Identical, because it is the same code on the same path: the control loop, policies, safety, fall
detection, kinematics, odometry, maploc, the whole IPC surface and its clients, the chorale's
election and beat, the systemd units with their real `User=`, groups, `RuntimeDirectory=` and
hardening, and the updater.

Modelled — the real code path, synthesised input: actuator response (BAM models fitted to the real
XL330s), the IMU, ToF depth, RSSI, the camera image, and release provenance on an x86 host.

Absent — not exercised at all: the Dynamixel bus driver, the BLE radio, the camera ISP and rkaiq's
3A, the NPU, the hardware encoder and its RGA path, thermals and battery.

**A useful check on that list:** run a week of real bugs past it. A `videoflip` that cost 22 fps by
breaking the encoder's zero-copy path to the RGA; a 3A engine missing a stream-start event; an
auto-exposure loop that converges once and stops; an INT8 head whose score channel collapses to two
values; a servo bus dropping reads. The twin would have caught **none** of them. That is not a flaw
in the design, it is the boundary of it — and hardware stays the only place the drivers are real.

## 7. The harness

One MuJoCo process, one window, N duck bodies in one scene, so ducks share physics and can bump into
each other. `microduck_rl` owns that half: it already has the scenes, the BAM actuator models and
mjlab, and serving a body to a daemon is the mirror of the sim2real it does today.

Each duck is a `systemd-nspawn` container — no daemon, no image format, a container is a directory —
booted from one Debian 13 Trixie rootfs (`mmdebstrap`, unprivileged, 238 MB, under three minutes)
with an overlay per duck. Run each as a service (`systemd-run --unit=duck-a …`) so `duck-sim down` is
a `systemctl stop`: a duck you cannot quit is not much of a duck.

`machinectl shell duck-a` is the way in, `duckctl open duck-a` the way to watch, and
`scripts/dev-push.sh microduck@duck-a` the way to install a build.

Two limits that follow from the physics rather than the plumbing. **Ducks do not hot-join** — MuJoCo
compiles its model, so changing the count restarts the simulator, which is why `RemoteIo` reconnects.
And **camera rendering is the scaling wall**, not the bipeds: N offscreen renders at 30 fps cost far
more than N fifteen-DoF bodies, so cameras should be opt-in per duck. The 45 Hz health gate makes
both a hard edge rather than a soft one — too many ducks and they do not get slow, they go
*unhealthy* and the updater starts rolling releases back.

## 8. Known material to reuse

`~/MISC/microduck_maploc` (outdated in every other respect) has two things worth taking: a simulated
VL53L5CX in `sim/tof_sensor.py` — 8×8 zones, 45° square FoV, 4 m range, noise that grows with
distance, 15 Hz — and `sim/assets/apartment.xml`, an indoor scene of 83 geoms. A room is worth much
more than a ground plane to maploc, to wander, and to anything that hides.
