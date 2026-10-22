# Guest → host protocol (v1)

The guest (`guest`, on Windows) tells the host (`host`, on Linux) what each
media session in the VM is playing; the host republishes it as MPRIS players.
The types live in the `protocol` crate; this document is the contract both
sides follow. Measurements behind the choices are in
wshowlyrics/smtc-mpris-bridge#1.

## Transport

- TCP. The guest connects to the host's address on the libvirt network (e.g.
  `192.168.122.1`); the host listens on that address only, never on all
  interfaces. Host address and port are configuration on both sides.
- UTF-8 [JSON Lines](https://jsonlines.org/): one message per line, `\n`
  terminated, at most 64 KiB per line.
- One direction only: the host never writes. Playback control (Play, Pause,
  Seek) is out of scope for v1.
- On disconnect the guest reconnects with backoff and starts over with `hello`.

## Messages

Every message is an object with a `type`. Receivers ignore unknown fields, so
fields can be added without a version bump; a message with an unknown `type`
is logged and skipped.

### `hello`

First message on every connection.

```json
{"type":"hello","v":1,"guest":"win11"}
```

| Field | Meaning |
|---|---|
| `v` | Protocol version. The host closes the connection on a version it does not speak |
| `guest` | Name of the VM, from the guest's configuration. The host uses it to tell guests apart |

### `state`

The full state of one media session. Every `state` replaces the previous one
for that `player`; nothing is incremental.

```json
{"type":"state","player":"AppleInc.AppleMusicWin_nzyj5cx40ttqa!App","status":"playing","track":{"title":"S.O.S","artists":["Indila"],"album":"Mini World","length_ms":272000.0},"position_ms":61211.7,"position_uncertainty_ms":6.6,"rate":1.0,"seek":false}
```

| Field | Meaning |
|---|---|
| `player` | SMTC `SourceAppUserModelId`, unique per app within a guest |
| `status` | `playing`, `paused` or `stopped` (see below) |
| `track` | Current track, or `null` while the app reports none (an empty title) |
| `track.title` | Title |
| `track.artists` | Artists; at least one entry |
| `track.album` | Album, or `null` |
| `track.length_ms` | Duration, or `null` when the app reports zero |
| `position_ms` | **Position at the moment the guest writes the line**, already corrected (see below) |
| `position_uncertainty_ms` | Half-width of the range the true position is known to be in, or `null` when the guest has no estimate |
| `rate` | Playback rate; `1.0` when the app reports none |
| `seek` | `true` when this state follows a seek that has settled. Optional, `false` when absent |

`status` maps SMTC's six values to three: `Playing` → `playing`, `Paused` →
`paused`, `Stopped`/`Closed`/`Opened` → `stopped`. `Changing` is transient and
is not sent; the guest waits for the next stable value.

### `gone`

The session closed (the app quit). The host removes the player.

```json
{"type":"gone","player":"AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"}
```

## Guest rules

### Position

Apple Music reports positions truncated to whole seconds while stamping each
update with the current time, so `Position + (now − LastUpdatedTime)` runs 0–1 s
late. The guest instead tracks the playback origin from the second boundaries
it sees (`PhaseEstimator`) and sends the estimate for the moment of sending:
about ±50 ms three seconds into playback, about ±10 ms after 25 seconds
(#1, note 4534). The estimate restarts on pause, track change and seek.

`position_uncertainty_ms` is the estimator's half-width, but never below the
jitter observed since the last restart: a range collapsed to a point is not
exact. Before the first timeline update of a play, the guest sends the raw
position with `position_uncertainty_ms: null`.

### Metadata

Apple Music puts `Artist — Album` in both `Artist` and `AlbumArtist` and leaves
`AlbumTitle` empty. When `AlbumTitle` is empty and `Artist` contains ` — `
(U+2014 with a space on each side), the guest splits at the **first**
occurrence: the left side is the artist, the right side the album. Artist names
rarely contain the dash; album names are more likely to.

Empty strings become `null` (`album`) or make `track` `null` (empty `title`).
Multiple artists are not split: SMTC gives one string.

### When to send

| Trigger | Sent |
|---|---|
| Connection established | `hello`, then one `state` per session |
| Status or track changed | `state` at once (after the media properties for a new track arrive) |
| Seek settled | `state` with `seek: true` |
| Playing | `state` every 1 s (heartbeat) |
| Paused or stopped | `state` every 5 s (keepalive) |
| Session closed | `gone` |

A seek is **settled** once two consecutive timeline updates agree with the
restarted estimate, or 500 ms pass without another update. Until then the guest
sends nothing for that player: Apple Music flips between old and new positions
for up to ~0.8 s while seeking (#1, note 4524), and forwarding that would make
lyrics jump back and forth.

## Host rules

- On each `state`, note the local monotonic time of receipt. While `playing`,
  MPRIS `Position` is `position_ms + (now − received) × rate`; otherwise it is
  `position_ms`. Network delay on the libvirt bridge is well under a
  millisecond and is ignored.
- Emit MPRIS `Seeked` for `seek: true`, and also when a heartbeat's position is
  more than 1 s away from the extrapolated one (a seek the guest missed).
- Publish one MPRIS player per (`guest`, `player`) pair.
- Remove a connection's players when it closes, or when nothing arrives on it
  for 15 s (three missed keepalives).
