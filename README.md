# smtc-mpris-bridge

Shows what a Windows VM is playing (Apple Music, or anything else that
publishes to the Windows media session, SMTC) as an MPRIS player on the Linux
host, so MPRIS clients such as [wshowlyrics](https://github.com/wshowlyrics/wshowlyrics)
can follow it.

| Part | Runs on | Status |
|---|---|---|
| `guest` | Windows 10 1809+ / 11 | `run` sends to the host; `probe` logs SMTC |
| `host` | Linux | publishes each guest session as an MPRIS player |
| `protocol` | both | v1 message types; the contract is [docs/protocol.md](docs/protocol.md) |

Work is tracked in the `wshowlyrics` group epic &1 on the self-hosted GitLab;
this GitHub repository is a mirror.

## Sending to the host (`run`)

```powershell
.\smtc-mpris-bridge-guest.exe run --host 192.168.122.1 [--port 47800] [--guest NAME]
```

connects to the host, sends what each SMTC session plays as described in
[docs/protocol.md](docs/protocol.md), and reconnects with backoff when the
connection drops. `--guest` names the VM to the host and defaults to the
computer name. Connections and errors go to stderr; Ctrl+C stops it.

Port 47800 must be open on the host's `virbr0` (see
[Copying to the guest](#copying-to-the-guest) for the firewall). To see the
stream itself instead of the host daemon, listen with OpenBSD `nc` (on NixOS,
`nix-shell -p netcat-openbsd`):

```bash
nc -lk 192.168.122.1 47800
```

### Starting at logon

SMTC sessions are only visible inside the interactive session of the user who
plays the music, so the bridge cannot run as a service; a scheduled task that
starts at that user's logon is the fit. After a reboot it starts once the user
logs on.

Run the command directly in a terminal first to see that it connects, then
put the exe in a fixed place (the task breaks if the file moves) and register
the task in PowerShell as that user:

```powershell
New-Item -ItemType Directory -Force $HOME\smtc-mpris-bridge | Out-Null
Copy-Item $HOME\Downloads\smtc-mpris-bridge-guest.exe $HOME\smtc-mpris-bridge\

$exe = "$HOME\smtc-mpris-bridge\smtc-mpris-bridge-guest.exe"
$action = New-ScheduledTaskAction -Execute conhost.exe -Argument "--headless `"$exe`" run --host 192.168.122.1"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName smtc-mpris-bridge -Action $action -Trigger $trigger -Settings $settings
```

- `conhost --headless` runs it without a console window, so its log is not
  visible; that is why the terminal run comes first.
- Task Scheduler stops a task after 3 days by default; `ExecutionTimeLimit`
  zero removes the limit for a VM that stays up.
- `RestartCount`/`RestartInterval` restart it if it fails. An unreachable host
  is not a failure: the bridge keeps reconnecting, backing off to 30 s.

Start it without rebooting, check it, stop it (e.g. before replacing the exe,
which cannot be overwritten while it runs) or remove it:

```powershell
Start-ScheduledTask -TaskName smtc-mpris-bridge
Get-ScheduledTask -TaskName smtc-mpris-bridge | Select-Object State   # Running
Stop-ScheduledTask -TaskName smtc-mpris-bridge
Unregister-ScheduledTask -TaskName smtc-mpris-bridge -Confirm:$false
```

## Host daemon

```bash
smtc-mpris-bridge-host [--config FILE] [--listen ADDR[:PORT]] [--allow APP]... [--ignore APP]...
```

listens on `192.168.122.1:47800` by default, only on that address, and
publishes each session of each connected guest as its own MPRIS player:
`org.mpris.MediaPlayer2.smtc_mpris_bridge.<guest>.<app>`, e.g.
`smtc_mpris_bridge.DESKTOP_G2NP3G0.AppleMusicWin` in `playerctl -l`. A player
goes away when its app closes, when the guest disconnects, or after 15 s
without a message. Playback control is not supported.

### Configuration

`~/.config/smtc-mpris-bridge/host.ini` (or under `$XDG_CONFIG_HOME`; `--config`
picks another file). Without it every app is published on the default address.

```ini
[host]
listen = 192.168.122.1:47800

[players]
# Comma-separated app names (AppleMusicWin) or full app ids. Empty: every app.
allow = AppleMusicWin
# Taken out of what allow lets through.
ignore =
```

`allow` keeps apps that come and go in the guest from turning into host
players; `ignore` suits the opposite case, e.g. leaving out `SpotifyMusic` when
the host's own Spotify already shows the guest's playback through Spotify
Connect. The command-line options override the file, a list replacing the
file's list, and keep doing so across reloads. Unknown sections and keys are
errors, so a typo does not quietly publish everything.

`systemctl --user reload smtc-mpris-bridge-host` (SIGHUP) rereads the file:
players no longer allowed go away at once, newly allowed apps appear with
their next state (within 5 s). A file that does not parse keeps the previous
settings, and a new `listen` needs a restart.

### Installing as a user service

```bash
cargo install --path host --root ~/.local   # on NixOS: inside nix-shell
mkdir -p ~/.config/systemd/user
cp host/smtc-mpris-bridge-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now smtc-mpris-bridge-host
journalctl --user -u smtc-mpris-bridge-host -f
```

The daemon binds with `IP_FREEBIND`, so it starts at login even before libvirt
has brought up `virbr0`. On NixOS, a binary built in `nix-shell` refers to
store paths a garbage collection can remove; rebuild it after one.

### With wshowlyrics

- `preferred_players = smtc_mpris_bridge` in `[lyrics]` matches every guest
  player.
- The players carry no `xesam:url`, so local lyrics are found only by the
  `<artist> - <title>.<ext>` name patterns (e.g. in `~/.lyrics/`), otherwise
  from lrclib.
- Delay in the guest's audio path is corrected with `global_offset_ms`.

## Measuring SMTC (`probe`)

Before the bridge sends anything, `probe` records what each app publishes to
SMTC, to decide the protocol (wshowlyrics/smtc-mpris-bridge#1):

```powershell
.\smtc-mpris-bridge-guest.exe probe [--out-dir logs] [--interval 1] [--slack-ms 100]
```

Each session event, a sample every `--interval` seconds and any line typed on
stdin go to `logs\smtc-<unix time>.jsonl`. Type what the player shows (e.g.
`1:23`) and press Enter to put a mark next to the computed position. `q` or
Ctrl+C writes a summary per app:

- `update_interval_ms` — how often the app refreshes its timeline
- `residual_ms` — how far each update lands from the previous one extrapolated
  to its `LastUpdatedTime`; near zero means the timestamp can be trusted
- `arrival_age_ms` — how old an update already is when its event arrives
- `seeks` — position jumps, kept out of `residual_ms`

Apple Music reports positions truncated to whole seconds, so `probe` also
recovers the sub-second position from the boundaries it sees (`est_ms` in each
row; conflicts up to `--slack-ms` count as jitter, larger ones as seeks):

- `estimator.half_width_ms` — remaining uncertainty of `est_ms`
- `estimator.edge_miss_ms` — how far a predicted second boundary fell outside
  the window in which the app showed it; zero means the estimate held
- `estimator.jitter_gap_ms` — conflicts absorbed as jitter

## Building

The host daemon builds on Linux. The guest builds either on that same Linux
host (cross-compiled, then copied into the VM) or inside the Windows guest
itself.

### On Linux: host daemon, and the guest cross-built

Needs [rustup](https://rustup.rs/) and the mingw-w64 cross compiler
(`gcc-mingw-w64-x86-64` on Debian/Ubuntu, `mingw-w64-gcc` on Arch,
`mingw64-gcc` on Fedora) as the Windows linker:

```bash
rustup target add x86_64-pc-windows-gnu   # once
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc

cargo build -p smtc-mpris-bridge-host --release
cargo build -p smtc-mpris-bridge-guest --release --target x86_64-pc-windows-gnu
cargo test   # platform-independent parts of every crate
```

On NixOS, `shell.nix` provides rustup, the linker and that variable: run the
same commands as `nix-shell --run '…'`.

Name the package (`-p`) for the Windows target: the workspace also holds the
Linux-only host, which does not build for Windows. The resulting
`target/x86_64-pc-windows-gnu/release/smtc-mpris-bridge-guest.exe` imports
only system DLLs, so copying that one file to the guest is enough.

#### Copying to the guest

Serve the build output on the libvirt network only (check the address with
`ip -4 addr show virbr0`; `192.168.122.1` is libvirt's default):

```bash
cd target/x86_64-pc-windows-gnu/release
python3 -m http.server 8000 --bind 192.168.122.1
```

and fetch it from the guest:

```powershell
Invoke-WebRequest http://192.168.122.1:8000/smtc-mpris-bridge-guest.exe -OutFile $HOME\Downloads\smtc-mpris-bridge-guest.exe
```

If the guest cannot connect, the host firewall is dropping the port on
`virbr0`; allow TCP 8000 there in whatever firewall the host runs. On NixOS
this opens it until the next reboot or `nixos-rebuild`:

```bash
# iptables backend (the default)
sudo iptables -I nixos-fw -i virbr0 -p tcp --dport 8000 -j nixos-fw-accept
# nftables backend (networking.nftables.enable = true)
sudo nft insert rule inet nixos-fw input-allow iifname virbr0 tcp dport 8000 accept
```

To keep it open, declare it in the NixOS configuration instead; this works
with either backend:

```nix
networking.firewall.interfaces.virbr0.allowedTCPPorts = [ 8000 ];
```

The bridge itself needs port 47800 the same way (`--dport 47800` /
`tcp dport 47800`, or `47800` in that list).

### On Windows: the guest, built inside the VM

Needs [rustup](https://rustup.rs/) with its default MSVC toolchain, which in
turn needs the Visual Studio Build Tools ("Desktop development with C++");
the rustup installer offers to set them up. Then, in the guest:

```powershell
git clone https://github.com/wshowlyrics/smtc-mpris-bridge
cd smtc-mpris-bridge
cargo build -p smtc-mpris-bridge-guest --release
```

The exe is `target\release\smtc-mpris-bridge-guest.exe`, with its `.pdb` next
to it; keep both, since a crash dump only shows function names with the
`.pdb`. An MSVC build links the Visual C++ runtime dynamically, so another
Windows machine may also need the Visual C++ Redistributable.

Stop the scheduled task before rebuilding or replacing the exe: Windows does
not let a running exe be overwritten.

```powershell
Stop-ScheduledTask -TaskName smtc-mpris-bridge
cargo build -p smtc-mpris-bridge-guest --release
Start-ScheduledTask -TaskName smtc-mpris-bridge
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
