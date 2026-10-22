# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

A bridge that shows what a Windows VM (libvirt/QEMU guest) is playing as MPRIS players on the Linux host, so wshowlyrics (which reads only host MPRIS via playerctl) can follow Apple Music running in the guest. The guest reads Windows SMTC and sends JSON Lines over TCP; the host daemon republishes them on the session bus.

## Build, Lint, Test

NixOS: `shell.nix` provides rustup and the mingw-w64 cross linker. Everything runs inside it.

```bash
nix-shell --run 'rustup target add x86_64-pc-windows-gnu'     # once
nix-shell --run 'cargo test'                                   # platform-independent parts
nix-shell --run 'cargo test -p smtc-mpris-bridge-guest player::tests::a_quiet_seek_settles_after_500_ms'
nix-shell --run 'cargo fmt --check'
nix-shell --run 'cargo clippy --all-targets -- -D warnings'
nix-shell --run 'cargo clippy -p smtc-mpris-bridge-guest --target x86_64-pc-windows-gnu --all-targets -- -D warnings'
nix-shell --run 'cargo build -p smtc-mpris-bridge-guest --release --target x86_64-pc-windows-gnu'
```

**Run clippy for both targets.** `probe.rs`, `run.rs` and `smtc.rs` are `#[cfg(windows)]`, so the Linux clippy and `cargo test` never compile them; only the Windows-target clippy does. The exe (`target/x86_64-pc-windows-gnu/release/smtc-mpris-bridge-guest.exe`) imports only system DLLs and cannot run on Linux. Checking real behaviour means copying it into the VM (README "Copying to the guest") and reading the stream on the host with `nc -lk 192.168.122.1 47800`. Port 47800 (and 8000 for the copy) must be open on `virbr0`; the README has the iptables/nftables/NixOS options.

The host daemon can be checked end to end without a VM and without touching the user's desktop: run it under `dbus-run-session` (an isolated session bus), `--listen 127.0.0.1:<port>`, write protocol lines to it from bash `/dev/tcp` or `nc`, and query with `playerctl`/`busctl --user`. Start background helpers (e.g. `dbus-monitor`) **before** opening the socket fd in the script, or they inherit it and the connection never closes. `playerctl metadata` skips players whose `CanPlay` is false, which is why the host reports `CanPlay = true` while `CanControl` stays false.

## Architecture

Cargo workspace: `guest` (Windows binary), `host` (Linux daemon) and `protocol` (message types shared by both). The crate is named `smtc-mpris-bridge-protocol` and imported as `protocol` (`guest/Cargo.toml`).

### The protocol contract

`docs/protocol.md` is the contract and the `protocol` crate is its only implementation. Its tests encode the doc's JSON examples **byte for byte**, so a change to field names, order or types must update both. Timing constants (`HEARTBEAT_*`, `HOST_TIMEOUT`, `SEEK_THRESHOLD_MS`, `DEFAULT_PORT`) live in the crate, not in either side.

### Guest: WinRT at the edges, logic in testable modules

The guest keeps WinRT out of everything that decides anything, so the decisions are unit-tested on Linux:

- `smtc.rs` (Windows) — the only WinRT code shared by both modes. `Smtc<M>` subscribes to every session and forwards events into the mode's own mpsc message type (`wrap: fn(Event) -> M`). WinRT handlers run on threadpool threads; all state lives on the single-threaded main loop. `read()` returns one session's status/rate/position/end/`LastUpdatedTime`.
- `timeline.rs` (portable) — WinRT time conversions and the position estimator (`PhaseEstimator`, wrapped by `Tracker`).
- `player.rs` (portable) — per-session send rules: what a `state` says and when it is due (status mapping is in `run.rs`, everything else here).
- `run.rs` (Windows) — the `run` mode: TCP `Link` with backoff, `Bridge` main loop (heartbeats, settle polling, `gone` on session close).
- `probe.rs` (Windows) + `stats.rs` — the `probe` measurement mode: logs every event/sample to `logs/smtc-*.jsonl` with a per-app summary. `stats` is probe-only; do not use it from `run`.

Portable modules carry `#[cfg_attr(not(windows), allow(dead_code))]` in `main.rs` because on Linux only their tests use them.

### Why the position logic looks the way it does

Measured on Apple Music in #1 (notes 4524, 4534); the rules are in `docs/protocol.md` "Guest rules":

- Apple Music **truncates positions to whole seconds** and restamps `LastUpdatedTime` every ~272 ms, so `Position + (now − LastUpdatedTime)` runs 0–1 s late. `PhaseEstimator` intersects the origin intervals implied by each truncated report (±50 ms after 3 s, ±10 ms after 25 s); conflicts up to the slack (100 ms) are jitter and set a floor on the reported uncertainty.
- Seeks flap between old and new positions for ~0.8 s, so a seek is held back until two updates agree or 500 ms pass, then sent once with `seek: true`. Restarts that move the estimate less than `SEEK_THRESHOLD_MS` are not seeks.
- Extrapolate the raw position only from a timeline stamped while playing (otherwise a resume adds the whole pause). A track change drops the old position and length.
- Metadata: `Artist` holds `Artist — Album` with `AlbumTitle` empty (split at the first ` — `); an empty artist is Apple Music's loading placeholder (`연결 중…`) and becomes `track: null`.

### Host: one D-Bus connection per player

`host` is tokio (current-thread) + zbus **6** — zbus 6 merged zvariant into `zbus::wire` and renamed `Value`/`OwnedValue` to `Variant`/`OwnedVariant`, and connection builder setters return `Self`, so zbus 5 examples do not apply as written.

- `guest.rs` — one task per TCP connection: `hello` first, lines capped at `MAX_LINE_BYTES`, `HOST_TIMEOUT` per read; on close or timeout every player of that connection is released.
- `playback.rs` (D-Bus-free, tested) — the last state, `position_ms(now)` extrapolation, and which signals a new state calls for (`Seeked` on `seek: true` or a heartbeat more than `SEEK_THRESHOLD_MS` off).
- `mpris.rs` — `GuestPlayer` owns its **own zbus connection**: every MPRIS player sits at `/org/mpris/MediaPlayer2`, so two cannot share one. Names are requested with allow/replace flags so a reconnecting guest takes its names over from the stale connection. `Position` is computed on read and never signalled (MPRIS rule; wshowlyrics polls it with `Properties.Get`).
- `names.rs` — `org.mpris.MediaPlayer2.smtc_mpris_bridge.<guest>.<app>`; wshowlyrics' `preferred_players` and playerctl match `smtc_mpris_bridge` up to the `.`.
- `config.rs` — `host.ini` (hand-written INI parser, strict about unknown keys) and the `allow`/`ignore` filter. `main.rs` lays CLI options over it and rereads it on SIGHUP; the filter reaches every connection through a `watch` channel, and a change drops players that are no longer allowed.

Players are removed rather than left `Stopped` on disconnect: wshowlyrics switches players on `NameOwnerChanged`, not on a stopped status.

Known gaps: the estimate wobbles right after a track starts (#6), and any position that is a multiple of 1000 (e.g. a track start at 0) feeds the estimator, so apps that do not truncate, like Spotify, get a wrong estimate (#7).

## Repository and Issues

- **`origin` is the self-hosted GitLab** (`wshowlyrics/smtc-mpris-bridge`, project id 44); `github.com/wshowlyrics/smtc-mpris-bridge` is an auto push mirror — never push it manually. Use `glab`.
- Issues are children of the `wshowlyrics` group epic &1 (group id 76): add new ones with `glab api groups/76/epics/1/issues/<issue global id> -X POST`.
- Labels: the shared axes (type, priority, severity, `blocked:*`, `hold::*`, follow-up, `ci`, `security`) are **group labels**, documented in wshowlyrics' CLAUDE.md "Issue Labels"; read `glab label list` for the judging rules. `component:*` are **project labels** of this repo only (`guest:windows`, `host:mpris`, `protocol`; #4) — create new ones with `POST projects/44/labels`, never on the group.
- **Issue titles carry no area prefix** (`게스트:`, `ci:` …); the area is the `component:*`/`ci` label (#4).
- Weight follows wshowlyrics/wshowlyrics#40.
- Commits: `type(scope): Subject` with scope `guest`/`protocol`/`host`, GPG-signed.
