# Why a gamepad will not stay bonded to some boards

Two separate faults produced one symptom. One was in this tree and is fixed. The
other is not, and its cause is still unknown.

The boards involved: the adapter that fails is `50:37:CD:16:2A:39`, the reference
that works is `50:37:CD:16:1B:92`. Roughly half of ten Radxa Zero 3W units show the
symptom.

## Fault 1 — nothing answered the pairing confirmation

`pad pair` failed with `org.bluez.Error.AuthenticationCanceled` after a 17-second
stall:

```
SMP: Pairing Request    Bonding, MITM, SC, No Keypresses, CT2 (0x2d)
SMP: Pairing Response   Bonding, No MITM, SC, No Keypresses (0x09)
MGMT: User Confirmation Request   Confirm hint: 0x01
Disconnect              Reason: Connection Timeout (0x08)
```

bluetoothd pushes an IO capability down to the adapter from the **default** agent
only. `configd` registered a scoped agent without claiming that role, so the adapter
kept the kernel's `DisplayYesNo`, and that capability puts MITM in the pairing
request. MITM makes SMP choose numeric comparison instead of just-works, and the
confirmation it raises reached no agent at all — `request_confirmation` was never
invoked, with or without `btd` running, because `btd` registers an agent only when
pairing is required and this board runs with it off.

Fixed by requesting the default agent for the pairing window. The request becomes
`Bonding, No MITM, SC, No Keypresses, CT2 (0x29)`, the agent is consulted, and an
Xbox pad that had never bonded on that board pairs first try.

## Fault 2 — the pad does not honour the stored key

With pairing completing cleanly, every reconnect still fails:

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

Thirty-four of thirty-five cycles in twenty seconds, no input device created, so
`padd` never sees a pad. Error `0x06` means the peripheral holds no long-term key for
this central. The bond written locally is well-formed — `Authenticated=2`,
`EncSize=16`, HID, DIS and Battery services discovered — so the key was stored here
and never took on the pad.

The ATT `0x0E` flood on every HID-over-GATT read, and the missing `[DeviceID]` and
`[ConnectionParameters]` sections in the bond, are downstream of a link that never
gets encrypted.

Reproduced on three different Xbox controllers, including one that had never touched
the board before. Board `1B:92` holds Xbox bonds on the same software without the
Fault 1 fix at all.

## What Fault 2 is not

| Hypothesis | How it was ruled out |
| --- | --- |
| Stale bonds, stale GATT cache | Wiped `/var/lib/bluetooth` for that adapter. Unchanged. |
| Wedged LE scanner | The controller was answering `Command Disallowed (0x0c)` to `LE Set Random Address` and `LE Set Extended Scan Parameters`, which BlueZ reports as `org.bluez.Error.InProgress`. Cleared by a reboot — a scan then returned 834 advertising reports. Unchanged. |
| Concurrency: second pad, advertiser | Single pad bonded, `btd` stopped. Unchanged. |
| Legacy vs Secure Connections | Cannot be chosen. Offered `Bonding, MITM, Legacy`, the pad answers `SC` and hangs up with `Remote User Terminated Connection (0x13)`, so `btmgmt sc off` is a dead end with an Xbox pad. |
| Authenticated pairing | After the Fault 1 fix the bond is just-works — `No MITM` on both sides. Unchanged. |
| Address privacy | Public address, `Privacy = off` in `main.conf`, no `LE Set Random Address` issued. |
| Divergent images | Two independently built cards: identical kernel `6.1.115-vendor-rk35xx`, Armbian 26.8.1, `aic8800 5.0+git20260123.5f7be68d-8`, BlueZ 5.82. |
| Chip variant or silicon revision | Both `0xC8A1:0x0082` / `0x0182`, firmware `8800d80_u02`, BT patch `Nov 06 2023 git 1f5d13b`, HCI and LMP 5.4 rev `0xb`. |
| Faulty AES engine | `LE Encrypt` with fixed inputs returns byte-identical ciphertext on both boards, with identical LE feature bits and supported states. |

## What still works on the failing board

A third-party pad bonded over LE on `2A:39` and survived a forced disconnect:

```
[LongTermKey]  Authenticated=0  EncSize=16
               EDiv=38034  Rand=13255886507827937191
> HCI Event: Encryption Change
        Status: Success (0x00)
        Encryption: Enabled with AES-CCM (0x01)
```

So storing a long-term key and re-encrypting a link with it does work on this
controller. That bond was legacy and unauthenticated, where the Xbox bond is LESC —
the two differ in more than one way, so this narrows the fault without naming it.

The same pad in another mode pairs over BR/EDR instead (`Encryption: Enabled with
E0`), which is a different transport and not a comparison for anything here.

## What is not known

Why Fault 2 follows the board. Every measurable property of the two units is
identical and no host-side variable is left. Whether it is silicon variance within
one revision, or something in the vendor BT firmware that a difference we have not
found triggers, is open.

## Reproducing

Read the adapter address first; the symptom follows the board, not the card.

```bash
hciconfig hci0 | grep -i "bd address"
```

With the pad on and flapping, the disconnect reason is the diagnosis:

```bash
sudo btmon | grep -A3 "Encryption Change"
```

`Status: PIN or Key Missing (0x06)` is Fault 2. `Status: Success` with
`Enabled with AES-CCM` is a healthy bond. A supervision timeout (`0x08`) is a range
or interference problem instead, and `scripts/pad-link-test.sh` measures that.
