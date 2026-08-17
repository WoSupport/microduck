# Pair a gamepad

Once per pad. After this, `padd.service` drives whatever pad is connected, from boot — nothing to
start, and nothing that dies with your ssh session.

## Put the pad in pairing mode

On an **Xbox** controller this is two presses, and the second is the one that goes wrong:

1. Switch it on with a **short** press of the Xbox button. Do not hold that button — held, it
   switches the controller off.
2. Press the small **Sync** button on the top edge, next to the USB-C port, until the Xbox light
   **flashes quickly**. Slow blinking means it is on but not pairing.

On a **DualSense**: hold Create and PS together until the light bar flashes.

## Pair it

```bash
sudo robotctl pad pair
```

```
looking for a gamepad in pairing mode — on an Xbox pad, press the small Sync button on the
top edge (not the Xbox button, which switches it off)
paired  Xbox Wireless Controller 78:86:2E:BB:13:28
padd is driving from it now.
```

No MAC address needed: the robot looks for a gamepad in pairing mode and takes the one it finds. The
pad is *trusted* as well as paired, which is what makes it reconnect by itself after a reboot with
nobody logged in.

If two are in pairing mode it refuses rather than guessing and prints their addresses. Naming one is
also how to pair hardware the robot does not recognise as a gamepad:

```bash
sudo robotctl pad pair 78:86:2E:BB:13:28
```

**A second pad needs no forgetting.** A pad already bonded is in range and in every sweep, so the
robot prefers one in pairing mode; both stay paired afterwards and `padd` drives whichever connects.
The cost is that re-running with nothing new in pairing mode waits out the whole search window
before reporting the pad you already have — `--timeout 5` if you are only repairing trust.

## Check it

```bash
robotctl pad status
```

```
pad     Xbox Wireless Controller 78:86:2E:BB:13:28  connected
padd    active — driving whatever pad connects
```

Two lines, because they fail separately: a connected pad with a dead driver looks exactly like a
working robot ignoring you.

`paired but NOT trusted` is the state worth knowing. It works now and does not reconnect after a
reboot, because approving a reconnection needs an agent and at boot there is none. Re-run `pad pair`
to fix it.

## Forget one

```bash
sudo robotctl pad forget 78:86:2E:BB:13:28
```

This removes **the robot's half** of the bond, which is all a robot can remove. The pad keeps its own
half, so pairing it again needs it back in pairing mode — otherwise it arrives with a key this robot
no longer has and the bond is refused.

## When pairing fails every time

Check `/etc/bluetooth/main.conf` for `Privacy = device`. Boards provisioned before this was
understood have it, and with it **a pad cannot bond at all**: it rejects the pairing with `DHKey
check failed (0x0b)`, because that check is computed over both devices' addresses and privacy pairs
from a resolvable private one. `Privacy = off` is what works.

```bash
sudo sh scripts/setup-board.sh
```

```bash
sudo reboot
```

`setup-board.sh` corrects the value, and it does not take effect until the reboot.

## When it pairs, works, and then loops connect/disconnect ever after

The signature, in `sudo btmon`:

```
< HCI Command: LE Start Encryption ... Long term key[16]: ...
> HCI Event: Encryption Change: Status: PIN or Key Missing (0x06)
```

The robot holds a bond and asks to encrypt with it; **the pad answers that it has no such
key**. The robot's half is fine — the pad lost its half, which old Xbox firmware (5.0x era)
genuinely does **at every one of its own power-offs**. Proven end to end on a real pad
(firmware 5.9) with a full `btmon` capture: a flawless SC pairing, `Encryption: Enabled
with AES-CCM`, the robot storing exactly the negotiated LTK — and the pad, power-cycled
five seconds later, answering `PIN or Key Missing` to that same key. Reproduced identically
against a laptop, so no amount of robot-side work can fix it.

Why nobody noticed for months: under the old `Privacy = device` misconfiguration the
*robot* forgot every bond too, so the per-session Sync + pair everyone was doing anyway
masked the pad's amnesia completely. Fixing the robot's memory is what exposed it.

`pad.pair` heals this on its own: it verifies an existing bond against the pad
(connect + wait for `ServicesResolved`, which cannot happen without encryption) and re-pairs
fresh when the pad no longer honours it. So the recovery is always the same two steps —
press Sync, `sudo robotctl pad pair` — even when the robot believes it is already paired.

The *fix* is a pad firmware update (Xbox Accessories app, on Windows or an Xbox): recent
firmware keeps its bonds, and the robot's persisted half then means something. Until then a
pad on old firmware needs the Sync + `pad pair` dance once per pad power-up — exactly the
prototype-era workflow, which "worked" only because that robot never persisted bonds at all
and re-paired implicitly every time.

A pad that loops like this also drives fine over a **USB-C cable** into the robot (the
kernel ships `xpad`); `padd` picks it up like any pad. Useful when the firmware update has
to wait for a Windows machine.

## When it pairs and then drops every two seconds

A bonded pad that connects and disconnects in a loop — `bluetoothctl` showing `Connected: yes` /
`Connected: no` over and over — is **ERTM**, which Xbox controllers cannot cope with. Check:

```bash
cat /sys/module/bluetooth/parameters/disable_ertm     # must be Y
```

`N` means the fix never applied. The classic `/etc/modprobe.d/bluetooth.conf` line does nothing on
this board — the vendor kernel builds `bluetooth` in, and modprobe options do not reach built-in
code — which is why a board can carry the file for months and still loop. `setup-board.sh` writes
the sysfs parameter directly and persists it via tmpfiles.d; it takes effect for new connections
without a reboot. Power-cycle the pad once after flipping it.

(Historical note: `Privacy = device` was once credited with fixing this loop. It "fixed" it by
breaking bonding outright, so the loop never got the chance to start.)

Otherwise the usual cause is the pad having left pairing mode before the exchange began: press Sync
again and re-run while the light is still flashing quickly. To see the exchange itself:

```bash
sudo btmon -t > /tmp/btmon.log 2>&1 &
```

Pair, then `sudo pkill btmon` and look for `SMP: Pairing Failed` and the reason beside it. That is
the one instrument that distinguishes a board setting from a pad that is not listening.

---

Driving — the controls, the speed limits, and running `padd` from a laptop over a forwarded socket —
is in the [README](../../README.md#drive-it).
