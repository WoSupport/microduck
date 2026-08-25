# What CI spends, and on what

Status: decision wanted · Date: 2026-08-25 · Owner: pierre

Measured from the runs of 2026-08-25 (`gh run view --log`), not from reading the workflows.

## Nothing runs twice, but two workflows run

`ci.yml` fires **once** per push to a pull-request branch. Its `push` trigger is scoped to
`branches: [main]`, so a branch push reaches only the `pull_request` trigger — the classic
double-run (`push` *and* `pull_request` both firing on the same commit) is already avoided.

What looks like a double run is `ci` and `dev` starting within four seconds of each other:

```
ci   pull_request  webrtc-console  2026-08-25T15:41:32Z → 15:50:09Z
dev  push          webrtc-console  2026-08-25T15:41:29Z → 15:45:59Z
```

`dev.yml` triggers on `push: branches: ['**']`, so every push to every branch also
cross-builds, packages, signs and publishes an installable dev release.

The place something genuinely runs twice is **merging**. A merge to `main` starts `ci` (push)
and `dev` (push) again over a tree the pull request already proved green. On the free plan
there is no merge queue to collapse that.

## Where the time goes

Per push to an open pull request, three parallel `ci` jobs plus `dev`:

| job | wall clock | dominated by |
|---|---|---|
| `ci` / `board` | 425–540 s | `board-test.sh` (445 s) |
| `ci` / `check` | 278–464 s | apt (209 s), `cargo test` (128 s) |
| `ci` / `coverage` | 204–433 s | `cargo llvm-cov` (129 s), apt (85 s) |
| `dev` / `publish` | ~250 s | `cargo board --bins` (154 s), `xtask package` (54 s) |

Wall clock to a verdict is `board`, at seven to nine minutes. Runner minutes billed are the
sum: **roughly 25 per push.**

Inside `board-test.sh`, from the log timestamps:

```
cross-compile aarch64            108–152 s
mint fixtures, package             2–3 s
pull image, updater/socket/pad     11–14 s
setup-board.sh, first run        123–148 s   ←
setup-board.sh, 7 more runs       12 s each  ←
install.sh / postinstall checks    30–90 s
```

## The apt bill, twice over

**Inside the emulated container.** `setup-board.sh`'s `configure_audio` runs
`apt-get update -qq` and then installs `alsa-utils device-tree-compiler dkms gcc make
i2c-tools` — a real download and a real dpkg unpack, under QEMU, on the first invocation.
That is the 123–148 s. It then runs `apt-get update -qq` again for
`linux-image-vendor-rk35xx`, which does not exist in Debian's archive, so that half fails
and repeats on **every** invocation — eight of them, at ~12 s each.

Together that is **~210 s of the ~445 s board step**, and the board step is what everyone
waits for. Not one assertion in `board-test.sh` looks at apt: the checks read
`/boot/armbianEnv.txt`, `/stub/systemctl.log`, `/etc/bluetooth/main.conf` and the
`weird-ble` marker. `curl`, `find` and `systemctl` are already stubbed into `/stub`;
`apt-get` and `dpkg` are the two that were left real.

Stubbing them the way `systemctl` is stubbed — logging the arguments — removes the 210 s
*and* makes assertable something nothing currently covers: that `setup-board` asks for the
vendor kernel, and asks once.

The honest cost of the change: nothing would then exercise the branch where the vendor
kernel install succeeds. Nothing exercises it today either — in CI that install always
fails — so this loses no coverage that exists, only the appearance of it.

**On the host runners.** `check` and `coverage` each install the same four packages from
scratch, every run:

```
libudev-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev
```

209 s in `check`, 85 s in `coverage`, for identical package sets. `--no-install-recommends`
would cut the gstreamer dev trees substantially; caching the debs would cut it to the
restore time. Either is a straight win with no coverage question attached — about five
runner-minutes per push, and 209 s of `check`'s 464.

## The aarch64 build, built twice per push

`ci`'s `board` job runs, inside `board-test.sh`:

```
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.31 --bins
```

`dev`'s publish job runs `cargo board --bins`, which `.cargo/config.toml` defines as that
exact command. Same commit, same target, same profile, 108–154 s each, in two jobs with
separate `rust-cache` entries. Both then run `xtask package`.

They are not quite interchangeable: `ci.yml` sets `RUSTFLAGS: -D warnings` at workflow
level and `dev.yml` does not, so the fingerprints differ and one cache could not serve the
other as things stand.

Collapsing them means one workflow rather than two, which means one trigger. The shape that
works is the standard one: drop `pull_request` from `ci`, trigger everything on
`push: branches: ['**']`, and make the dev publish a job with `needs: board` that reuses
the binaries the board job already built. Wall clock for the dev release goes up (it waits
for the board checks) and total runner minutes go down by about four per push.

The trade-off is real. `pull_request` runs against `refs/pull/N/merge` — the merge result —
while `push` runs against the branch tip. Today's `ci` therefore tests what merging would
produce; a push-triggered one tests the branch alone, and a semantic conflict with `main`
would surface at merge instead of in the pull request. Fork pull requests also stop being
covered, though there are none. Against that: `board`'s cross-build is the largest single
compute item in the whole pipeline, and it is currently paid for twice on every keystroke.

## Publishing a release for every push

`dev.yml` publishes a signed, installable prerelease for every push to every branch,
including branches nobody will ever install. Four minutes, a release object and ~11 MB per
push, cleaned up later by `prune-dev-releases`.

The design rationale in [`ci-setup.md`](ci-setup.md) is `robotctl update apply daemon --ref
<branch>`, and it is worth having. Whether it is worth having on *every* push rather than on
the pushes someone intends to flash is a different question — a `[dev]` marker in the commit
message, or a label on the pull request, would keep the capability and skip most of the
builds. The nine `webrtc-console` pushes of that afternoon produced nine dev releases; one
was installed.

## Two smaller ones

`cargo test --workspace` (128 s in `check`) and `cargo llvm-cov --workspace` (129 s in
`coverage`) run the same suite twice, once plain and once instrumented. The comment in
`ci.yml` already argues for the split and the argument holds — they are parallel jobs, so it
costs no wall clock. It costs about two runner-minutes per push, which is the cheapest thing
on this page and the only one with a good reason attached.

`cargo install cargo-zigbuild --locked` in `board`, `dev` and `_build-release.yml` reports 0 s
today, because `rust-cache` carries `~/.cargo/bin`. On a cache miss it compiles from source.
`taiki-e/install-action@cargo-zigbuild` downloads a prebuilt binary and is already the
mechanism `coverage` uses for `cargo-llvm-cov`.

## What is not available

An arm64 hosted runner would delete the QEMU layer entirely rather than trimming what runs
inside it — the container phase of `board-test.sh` is emulated on `ubuntu-latest`, and the
job already carries `docker/setup-qemu-action` with a comment noting it is a no-op on arm64.
GitHub's arm64 runners are free for public repositories and a paid runner type for private
ones. `pollen-robotics` is on the free plan, so this is the same wall that stopped the
required-reviewers rule in [`ci-setup.md`](ci-setup.md).

## Ranked

| | change | saves | costs |
|---|---|---|---|
| 1 | Stub `apt-get`/`dpkg` in `board-test.sh`, logging args | ~210 s wall, ~3.5 min runner | a code path CI never reached anyway |
| 2 | Cache or trim the host apt install | ~4–5 min runner, ~180 s off `check` | nothing |
| 3 | One trigger, dev publish reuses `board`'s build | ~4 min runner | `ci` tests the branch, not the merge result |
| 4 | Dev release on a marker, not every push | ~4 min runner on most pushes | one word in a commit message to get a build |
| 5 | `taiki-e/install-action@cargo-zigbuild` | nothing today; minutes on a cache miss | nothing |

1, 2 and 5 have no design question in them. 3 and 4 change what CI promises, and are the
two worth deciding rather than just doing.
