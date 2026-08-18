# Why a gamepad will not stay bonded to some boards

Two separate faults produced one symptom. Both are now understood. One was in this tree and is
fixed. The other is not ours at all: it is the aic8800 driver bundled in Armbian's kernel package,
and the only fix is to change which kernel package the card carries.

The boards involved: `50:37:CD:16:2A:39` fails, `50:37:CD:16:1B:92` is the reference that works,
and `50:37:CD:16:1D:90` is where Fault 2 was finally cornered — it reproduced the symptom, and it
is the one board that has been run with two different SD cards and nothing else changed.

Roughly half of ten Radxa Zero 3W units show the symptom. That split is **not** silicon variance.
It tracks which kernel package the card happens to carry, and §"Fault 2" is the measurement that
shows it.

## Fault 1 — nothing answered the pairing confirmation

`pad pair` failed with `org.bluez.Error.AuthenticationCanceled` after a 17-second stall:

```
SMP: Pairing Request    Bonding, MITM, SC, No Keypresses, CT2 (0x2d)
SMP: Pairing Response   Bonding, No MITM, SC, No Keypresses (0x09)
MGMT: User Confirmation Request   Confirm hint: 0x01
Disconnect              Reason: Connection Timeout (0x08)
```

bluetoothd pushes an IO capability down to the adapter from the **default** agent only. `configd`
registered a scoped agent without claiming that role, so the adapter kept an IO capability that
declares input and display — and that capability is what puts MITM in the pairing request. MITM
makes SMP choose numeric comparison instead of just-works, and the confirmation it raises reached
no agent at all: `request_confirmation` was never invoked, with or without `btd` running, because
`btd` registers an agent only when pairing is required and this board runs with it off.

Fixed by requesting the default agent for the pairing window. The request becomes
`Bonding, No MITM, SC, No Keypresses, CT2 (0x29)`, the agent is consulted, and an Xbox pad that had
never bonded on that board pairs first try.

> **Correction to an earlier draft.** That draft said the adapter "kept the kernel's
> `DisplayYesNo`". The capability is not merely inherited — **bluetoothd issues it itself**, and it
> can be watched doing so. Restarting `bluetooth` with `btmon -w` running shows, in the same
> adapter-init burst as `Set Bondable` and before any of our daemons are up:
>
> ```
> @ MGMT Command: Set IO Capability (0x0018) plen 1   {0x0001} [hci0] 2.949573
>         Capability: KeyboardDisplay (0x04)
> ```
>
> On `1D:90` the value is `KeyboardDisplay (0x04)`, not `DisplayYesNo`, and registering a
> `NoInputNoOutput` agent afterwards does not change it. Both capabilities declare input and
> display and so both set MITM, which is why the mechanism above still holds and the fix still
> works — but anyone chasing the value should read `MGMT Set IO Capability` in a capture rather
> than reason about what the adapter "kept".

## Fault 2 — the radio driver, not the pad

With pairing completing cleanly, every reconnect still failed:

```
< HCI Command: LE Start Encryption
        Long term key[16]: 8992647c8f491820d58c9c53d7a1c635
> HCI Event: Encryption Change
        Status: PIN or Key Missing (0x06)
        Encryption: Disabled (0x00)
< HCI Command: Disconnect
        Reason: Authentication Failure (0x05)
```

On `1D:90` the same fault appears one step earlier, on the *first* bond rather than on reconnect —
the pairing runs to completion, encryption comes up, and the pad hangs up before it has distributed
its keys:

```
> HCI Event: Encryption Change (0x08)      #116  12.849550
        Status: Success (0x00)
        Encryption: Enabled with AES-CCM (0x01)
> HCI Event: Disconnect Complete (0x05)    #118  12.850376
        Reason: Remote User Terminated Connection (0x13)
```

**826 microseconds.** The connection interval is 50 ms, so nothing can have gone over the air in
between: the pad is not reacting to anything the host does after encryption, it quits at the same
connection event. No `SMP: Identity Information` ever arrives, so no `[LongTermKey]` is written and
BlueZ reports `AuthenticationCanceled`. Both shapes are the same fault — a bond that never commits —
seen from either side of a key that was never stored.

### What actually changes it

One board, one pad, two SD cards, nothing else touched:

| | old card — **works** | new card — **fails** |
|---|---|---|
| Armbian | 26.2.1 trixie | 26.8.1 trixie |
| `linux-image-vendor-rk35xx` | **26.5.1** | **26.8.1** |
| kernel | 6.1.115-vendor-rk35xx | 6.1.115-vendor-rk35xx |
| BlueZ | 5.82 | 5.82 |
| `aic8800_bsp` srcversion | `7ECB260741E5BC9FC904B92` | `738316A2E9D9825966BDB6B` |
| `aic8800_bsp` size in `lsmod` | 77824 | 86016 |
| result | bonds; `hid-generic … BLUETOOTH HID v5.09 Gamepad`, `/dev/input/js0` | quits 826 µs after `Encryption Change` |

The kernel **version string is identical**, which is most of why this hid for weeks: `uname -r`,
`bluetoothctl --version` and every fingerprint built from them match across a working and a broken
board. What differs is the aic8800 driver *build* bundled inside the kernel package.

Putting the 26.5.1 driver on the failing card fixes it outright. `sudo robotctl pad pair` then
succeeds first try with the full stack running:

```
paired  Xbox Wireless Controller 78:86:2E:90:93:3E
padd is driving from it now.

> HCI Event: Encryption Change (0x08)   Enabled with AES-CCM (0x01)
      SMP: Identity Information (0x08) len 16
      SMP: Identity Address Information (0x09) len 7
disconnect count: 0
```

`[PeripheralLongTermKey] Authenticated=2 EncSize=16`, `Trusted=true`, `/dev/input/js0`. The two SMP
packets in the middle are the ones that never arrived before.

**The firmware is not involved.** All five blobs under `/lib/firmware/aic8800/SDIO/aic8800D80/` are
byte-identical between the two images — `fmacfw` `2aa840eaea976e7fb87e33fd9e82a653`, `fw_adid`
`f546881a81b960d89a672578eb45a809`, `fw_patch` `9e3808a312cc19925259a6c5163753d5`,
`fw_patch_table` `0e6fd98c0a89c62ebd4c1a430fafa59f`, `lmacfw_rf` `501c4af180d187c0c5bf52b57cb27560`
— and the fix was re-verified with the newer firmware in place. Note that directory belongs to
`armbian-firmware`, not to the `aic8800-firmware` package, which owns `/lib/firmware/aic8800_fw/`.

## What Fault 2 is not

| Hypothesis | How it was ruled out |
| --- | --- |
| Stale bonds, stale GATT cache | Wiped `/var/lib/bluetooth` for that adapter. Unchanged. |
| Wedged LE scanner | The controller was answering `Command Disallowed (0x0c)` to `LE Set Random Address` and `LE Set Extended Scan Parameters`, which BlueZ reports as `org.bluez.Error.InProgress`. Cleared by a reboot — a scan then returned 834 advertising reports. Unchanged. |
| Concurrency: second pad, advertiser | Single pad bonded, `btd` stopped. Unchanged. |
| `btd` advertising while a peer is connected | Ruled out on `1D:90`: with `btd` stopped — `ActiveInstances: 0x00`, and zero `LE Set Extended Advertising Enable` anywhere in the capture — the failure is byte-for-byte identical. |
| Anything of ours at all | Ruled out on `1D:90`: with `btd`, `padd` **and** `configd` all stopped and the pairing driven entirely by `bluetoothctl` with its own `NoInputNoOutput` default agent, the pad still quit 818 µs after `Encryption Change`. |
| The MITM/IO-capability request | Ruled out independently of Fault 1: a capture holding both conditions shows `MITM (0x2d)` quitting after 792 µs and `No MITM (0x29)` — the Fault 1 fix's exact signature, agent consulted and answered — quitting after 1112 µs. |
| Legacy vs Secure Connections | Cannot be chosen. Offered `Bonding, MITM, Legacy`, the pad answers `SC` and hangs up with `Remote User Terminated Connection (0x13)`, so `btmgmt sc off` is a dead end with an Xbox pad. |
| Authenticated pairing | After the Fault 1 fix the bond is just-works — `No MITM` on both sides. Unchanged. |
| Address privacy | Public address, `Privacy = off` in `main.conf`, no `LE Set Random Address` issued. |
| The pad, its firmware, its batteries | The same pad bonds and drives on the same board the moment the card is swapped, and pairs to a laptop on demand. |
| Chip variant or silicon revision | Both `0xC8A1:0x0082` / `0x0182`, firmware `8800d80_u02`, BT patch `Nov 06 2023 git 1f5d13b`, HCI and LMP 5.4 rev `0xb`. And the same *unit* both works and fails depending only on its card. |
| Faulty AES engine | `LE Encrypt` with fixed inputs returns byte-identical ciphertext on both boards, with identical LE feature bits and supported states. |
| Divergent images | This was the misleading one. Two cards of the *same* Armbian generation are identical, which is what that comparison showed. The generation itself is the variable. |

### One row of an earlier draft was wrong

That draft said the ATT `0x0E` flood on every HID-over-GATT read, and the missing `[DeviceID]` and
`[ConnectionParameters]` sections, were "downstream of a link that never gets encrypted". The flood
is not. It appears on the **working** card too, while the pad is connected and driving with a live
`uhid` input device:

```
bluetoothd: hog-lib.c:output_written_cb() Write output report failed: … unlikely error
bluetoothd: hog-lib.c:info_read_cb() HID Information read failed: … unlikely error
bluetoothd: deviceinfo.c:read_pnpid_cb() Error reading PNP_ID value: … unlikely error
```

This pad produces those reads and those errors normally. They are noise, they cost hours, and they
should not be treated as a symptom of anything.

## Fixing a board

26.5.1 is in the public Armbian repo. A locally built kernel — 26.8.1 came from the internal build
server — is **not**, so this is one-way without a rebuild. Back up `/boot` and
`/lib/modules/$(uname -r)` first: the aic8800 driver carries WiFi as well as Bluetooth and the
Zero 3W has no wired link, so a driver that fails to load takes ssh with it.

```bash
sudo apt-get update
sudo apt-get install -y --allow-downgrades \
    linux-image-vendor-rk35xx=26.5.1 \
    linux-dtb-vendor-rk35xx=26.5.1 \
    linux-headers-vendor-rk35xx=26.5.1
sudo dpkg --configure -a
sudo apt-mark hold linux-image-vendor-rk35xx linux-dtb-vendor-rk35xx linux-headers-vendor-rk35xx
```

Expect the first `apt-get install` to fail configuring `linux-image`: dkms, pulled in by the headers
package, runs its autoinstall inside the postinst. `dpkg --configure -a` finishes it cleanly.

Then the step that is easy to miss, and without which **the downgrade changes nothing at all**.
`aic8800-sdio-dkms` stays at 5.0 — there is no 4.0 in any repo — dkms rebuilds it during the kernel
install, and its module in `updates/` outranks the good in-tree one:

```bash
cd /lib/modules/$(uname -r)/updates/dkms
for m in aic8800_bsp_sdio aic8800_btlpm_sdio aic8800_fdrv_sdio; do
    sudo mv "$m.ko" "$m.ko.disabled"
done
sudo depmod -a && sudo update-initramfs -u && sudo reboot
```

## Reading a board

`dpkg -l` is **not** a valid check — the kernel package can read 26.5.1 while the 5.0 dkms module is
the one loaded. Ask the running kernel instead:

```bash
cat /sys/module/aic8800_bsp/srcversion
```

| value | `lsmod` size | verdict |
| --- | --- | --- |
| `7ECB260741E5BC9FC904B92` | 77824 | good |
| `738316A2E9D9825966BDB6B` | 86016 | broken |

With the pad on and flapping, the disconnect reason is the diagnosis:

```bash
sudo btmon | grep -A3 "Encryption Change"
```

`Status: PIN or Key Missing (0x06)`, or `Status: Success` followed within a millisecond by
`Reason: Remote User Terminated Connection (0x13)`, is Fault 2. `Status: Success` with the link
still up — and `SMP: Identity Information` after it — is a healthy bond. A supervision timeout
(`0x08`) is a range or interference problem instead, and `scripts/pad-link-test.sh` measures that.

## What is not known

What changed in the aic8800 driver between the 26.5.1 and 26.8.x kernel builds. The regression is
in the driver module and nothing else — same firmware bytes, same kernel version, same BlueZ, same
silicon — but the specific change has not been bisected, and until it is, pinning the kernel is the
whole remedy.

The durable fix belongs in the image build rather than on each board: pin
`linux-image-vendor-rk35xx`, or find and carry the driver fix. Every card built from the newer
generation regresses otherwise.
