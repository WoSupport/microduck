# Why a gamepad will not stay bonded to some boards

Some Radxa Zero 3W units cannot keep an Xbox Wireless Controller bond. Pairing
succeeds, the pad drives for a few seconds, and then it reconnects and drops about
1.4 times a second, forever. No input device is ever created, so `padd` never sees a
pad. The same SD card in another board gives a steady connection.

Roughly half of ten boards behave this way. The failing unit here is the one whose
Bluetooth adapter is `50:37:CD:16:2A:39`; the reference that works is
`50:37:CD:16:1B:92`.

## What actually fails

The link establishes, and then the pad refuses to encrypt it with the key the robot
stored:

```
< HCI Command: LE Start Encryption
        Random number: 0x0000000000000000
        Encrypted diversifier: 0x0000
        Long term key[16]: 8992647c8f491820d58c9c53d7a1c635
> HCI Event: Encryption Change
        Status: PIN or Key Missing (0x06)
        Encryption: Disabled (0x00)
< HCI Command: Disconnect
        Reason: Authentication Failure (0x05)
```

ATT error `0x06` means the peripheral holds no long-term key for this central. The
robot's own bond record is well-formed — `Authenticated=2`, `EncSize=16`, HID, DIS
and Battery services all discovered — so the key was written locally but never took
on the pad.

Everything else that looks broken is downstream of that. The flood of
`Request attribute has encountered an unlikely error` (ATT `0x0E`) on every
HID-over-GATT read is BlueZ trying to initialise HOG over a link that never gets
encrypted, and the missing `[DeviceID]` and `[ConnectionParameters]` sections in the
bond are simply the artefacts a successful session would have left behind.

## What it is not

Each of these was eliminated by measurement, not reasoning.

| Hypothesis | How it was ruled out |
| --- | --- |
| Stale bonds, stale GATT cache | Wiped `/var/lib/bluetooth` for that adapter. Unchanged. |
| Wedged LE scanner | The controller was answering `Command Disallowed (0x0c)` to `LE Set Random Address` and `LE Set Extended Scan Parameters`, which BlueZ reports as `org.bluez.Error.InProgress`. Cleared by a reboot — a scan then returned 834 advertising reports. Unchanged. |
| Concurrency: second pad, advertiser | Single pad bonded, `btd` stopped. Unchanged. |
| LE Secure Connections key derivation | `btmgmt sc off` produced a genuine legacy bond with a transported key (`EDiv=0x0015`, non-zero `Rand`). Failed identically. |
| Authenticated pairing | `configd` already registers `NoInputNoOutput`, so pairing is Just Works. |
| Address privacy | Public address, `Privacy = off` in `main.conf`, no `LE Set Random Address` issued. |
| Divergent images | Two independently built cards: identical kernel `6.1.115-vendor-rk35xx`, Armbian 26.8.1, `aic8800 5.0+git20260123.5f7be68d-8`, BlueZ 5.82. |
| Chip variant or silicon revision | Both `0xC8A1:0x0082` / `0x0182`, firmware `8800d80_u02`, BT patch `Nov 06 2023 git 1f5d13b`, HCI and LMP 5.4 rev `0xb`. |
| Faulty AES engine | `LE Encrypt` with fixed inputs returns byte-identical ciphertext on both boards, with identical LE feature bits and supported states. |

## The test that located it

A different BLE gamepad, paired on the failing board, held its bond through a forced
disconnect:

```
[LongTermKey]  Authenticated=0  EncSize=16
               EDiv=38034  Rand=13255886507827937191
> HCI Event: Encryption Change
        Status: Success (0x00)
        Encryption: Enabled with AES-CCM (0x01)
```

Storing a long-term key and re-encrypting a link with it — the exact operation that
fails with Xbox pads — works on this board. So the controller's LE bonding and key
storage are sound, and the fault is an interop failure between this unit's Bluetooth
firmware and Xbox Wireless Controllers.

| Peer | Bond | Reconnect |
| --- | --- | --- |
| Xbox on `1B:92` | LESC, `Authenticated=2` | holds |
| Xbox on `2A:39` | LESC, `Authenticated=2` | `0x06` |
| Xbox on `2A:39` | legacy, `sc off` | `0x06` |
| BSP-G6 on `2A:39` | legacy, `Authenticated=0` | holds |

The one asymmetry left unexplained from the host side: the bond that works came out
`Authenticated=0` while the Xbox bond is `Authenticated=2` even with Secure
Connections disabled at the adapter. If the Xbox pad drives the security level up
regardless of what the robot offers, that belongs in a bug report against the
vendor firmware rather than in this tree.

## Consequences for the daemon

- A pad that negotiates an unauthenticated legacy bond works on every board tested,
  so pad selection is a way around this.
- `pad status` reported a pad as `paired, not connected` while BlueZ held an active
  LE connection to it.
- `pad pair` declares success on the pairing exchange alone. It announced a working
  pad for a bond that could not survive one reconnect.
- `configd` relays `org.bluez.Error.InProgress` verbatim, which reads as a competing
  scanner when the controller is refusing the command. Only a reboot clears that
  state — restarting `bluetoothd`, re-running `hciattach` and an `rfkill`
  block/unblock cycle all leave it in place, because `wlan0` keeps the shared
  `aicbsp` powered.
- Restarting `bluetooth.service` removes `hci0` entirely until
  `aic-bluetooth.service` is restarted.

## Reproducing

Read the adapter address first; the symptom follows the board, not the card.

```bash
hciconfig hci0 | grep -i "bd address"
```

With the pad on and flapping, the disconnect reason is the diagnosis:

```bash
sudo btmon | grep -A3 "Encryption Change"
```

`Status: PIN or Key Missing (0x06)` is this fault. `Status: Success` with
`Enabled with AES-CCM` is a healthy bond. A supervision timeout (`0x08`) is a range
or interference problem instead, and `scripts/pad-link-test.sh` measures that.
