# smtc-mpris-bridge

Shows what a Windows VM is playing (Apple Music, or anything else that
publishes to the Windows media session, SMTC) as an MPRIS player on the Linux
host, so MPRIS clients such as [wshowlyrics](https://github.com/wshowlyrics/wshowlyrics)
can follow it.

| Part | Runs on | Status |
|---|---|---|
| `guest` | Windows 10 1809+ / 11 | `probe` (log mode) only |
| `host` | Linux | not started |
| `protocol` | both | v1 message types; the contract is [docs/protocol.md](docs/protocol.md) |

Work is tracked in the `wshowlyrics` group epic &1 on the self-hosted GitLab;
this GitHub repository is a mirror.

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

The guest is cross-built from Linux with the mingw-w64 linker; on NixOS
`shell.nix` provides both:

```bash
nix-shell --run 'rustup target add x86_64-pc-windows-gnu'   # once
nix-shell --run 'cargo build --release --target x86_64-pc-windows-gnu'
nix-shell --run 'cargo test'                                 # platform-independent parts
```

The resulting `target/x86_64-pc-windows-gnu/release/smtc-mpris-bridge-guest.exe`
imports only system DLLs, so copying that one file to the guest is enough.

## Copying to the guest

Serve the build output on the libvirt network only (check the address with
`ip -4 addr show virbr0`; `192.168.122.1` is libvirt's default):

```bash
cd target/x86_64-pc-windows-gnu/release
nix-shell -p python3 --run 'python3 -m http.server 8000 --bind 192.168.122.1'
```

and fetch it from the guest:

```powershell
Invoke-WebRequest http://192.168.122.1:8000/smtc-mpris-bridge-guest.exe -OutFile $HOME\Downloads\smtc-mpris-bridge-guest.exe
```

If the guest cannot connect, the host firewall is dropping the port on
`virbr0`. On NixOS this opens it until the next reboot or `nixos-rebuild`:

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
