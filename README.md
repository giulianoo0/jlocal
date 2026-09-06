# jlocal

Local companion for [juntos.lol](https://github.com/giulianoo0/juntos.lol): torrent locally (no worker),
share screen up to 4K60 via the MoQ relay, all through one tiny native app.

> Loopback API + status/version window. Nothing else.

The app itself has no web UI. It shows two things:

- **status**: connected / not connected + capture permission (window title + console + `/capabilities` → `permissions.screenCapture`)
- **version**: the release tag (window title + `/version` + `--version`)

It also lives in the menubar tray (monotone icon): closing the window hides
to the tray instead of quitting; the tray menu has Open + Check-for-updates
+ Install-update (when one is known) + Quit.

## Quickstart

```sh
 cargo run -- --port 40392
 # jlocal v0.1.0 — status: connected (http://127.0.0.1:40392)

 curl http://127.0.0.1:40392/health
 # {"name":"jlocal","version":"v0.1.0","connected":true,"moq":"disabled","torrent":"standby","port":40392}

jlocal --version        # jlocal v0.1.0
jlocal --no-ui          # headless: API only (tray-less servers, CI)
```

## Loopback API (127.0.0.1 only, fixed port)

| Endpoint            | Description                                                      |
|---------------------|------------------------------------------------------------------|
| `GET /health`       | `{name, version, connected, moq, torrent, port}` (`torrent`: `live` when the engine booted) |
| `GET /version`      | `{name, version}`                                                |
| `GET /events`       | SSE: `hello` + 15s heartbeats.                                   |
| `GET /capabilities` | `{name, version, capabilities}` (`screen.capture`: frame capture live; `screen.available`: relay publish, still `false`; audio/torrent flags; `permissions.screenCapture`: OS capture consent, live probe) |
| `GET /audio/apps`   | `{apps:[{id,name}]}` when listable, else `501 {error}`           |
| `POST /audio/mode`  | `{mode}` persists `all`/`none`/`custom` (mute set preserved)     |
| `POST /audio/mute`  | `{app, muted}` persists one app toggle                           |
| `GET /capture/displays` | `{displays:[{id,name,width,height}]}` from the OS           |
| `GET /capture/windows` | `{windows:[{id,name,width,height}]}` from the OS (minimized/zero-area skipped) |
| `GET /capture/preview.jpg` | Latest frame JPEG, `404 {error:"idle"}` when stopped     |
| `GET /capture/snapshot` | One-frame JPEG for picker previews: `?display_id=<id\|empty=primary>` xor `?window_id=<id>`, `&width=<px>` (default 960, clamp 160–1920, aspect kept); `200 image/jpeg`; `400` both/neither/garbage/unknown id; `503 {error:"permission"\|"unavailable"}` (stateless — never touches the running session) |
| `POST /capture/start` | `{display_id _xor_ window_id,width,height,fps}` → `{started,display_id\|window_id,width,height,fps}` (exactly one id; unknown id is `400`; a window closed mid-session ends frames so preview `404`s; preview only — publish unwired, see below) |
| `POST /capture/stop`  | `{stopped:true}` (idempotent, always 200)                      |
| `POST /torrent/add` | `{id, name}` (waits ≤60s for magnet metadata; `504` on timeout)  |
| `GET /torrent/list` | `{torrents:[{id,name,size,progress,state,downBps,files}]}`        |
| `GET /torrent/data/{id}/{file}` | `206` ranged bytes (`Range`, index or path; `416` + `Content-Range: bytes */total` when unsatisfiable; single reads capped at 64 MiB) |
| `POST /torrent/select` | `{selected:true}` (focuses the swarm on one file)             |
| `GET /torrent/stats/{id}` | `{peers,downBps,downloaded,progress}`                       |
| `DELETE /torrent/{id}` | `{removed:true}` (refuses metadata-less torrents: retry later) |

Security: `Host` must be loopback (DNS-rebinding guard); CORS echoes
allowlisted origins (`Access-Control-Allow-Origin` + Vary) on every endpoint
including `GET /health`; `Access-Control-Allow-Private-Network: true`;
`Cache-Control: no-store`. Browsers should `fetch` (not `EventSource`).

Torrent bytes download into the OS temp dir (`jlocal-torrents`); session
state persists there for fastresume. No UPnP: the app never punches NAT holes.
Reads ahead of the download block on the swarm (no prefetch beyond the
engine's lookahead) and fail after 90s per read (`504`).

Screen publish is still unwired: the relay speaks MoQ draft-16 and no Rust
crate negotiates it (draft-18+/moq-lite only), so `screen.available` stays
`false` and the web UI keeps routing to the browser picker. Capture
endpoints above are live (real frames, advertised as `screen.capture`),
ready for a transport when one exists.

Env: `JLOCAL_PORT` (default `40392`), `JLOCAL_NO_UI`, `JLOCAL_ALLOWED_ORIGINS`
(comma-separated `https://` origins, replaces the juntos.lol + beta defaults),
`JLOCAL_VERSION` (injected by release CI).

## Self-update

On boot and every 6h the app checks
`https://api.github.com/giulianoo0/jlocal/releases/latest` (10s timeout,
best-effort, never blocks boot). When the tag is newer than its own build
it shows `update available vX` in the window title and enables the tray's
`Install update vX` item (plus a manual `Check for updates now` item).
Install downloads the release asset for the OS (`jlocal-<tag>-<target>.tar.gz`
on macOS/Linux, `.zip` on Windows — the exact names `release.yml` uploads),
swaps its own executable (inside `JLocal.app` it replaces
`Contents/MacOS/jlocal` and re-opens the bundle), and relaunches. No forced
updates, and no signature verification beyond HTTPS.

## Install (per OS)

Binaries ship on every `v*` tag via GitHub Actions (see Releases).

- **Windows**: download `jlocal-<ver>-x86_64-pc-windows-msvc.zip`, unzip, run
  `jlocal.exe`. No installer in 0.1.x. SmartScreen may warn (unsigned) →
  *More info → Run anyway*.
- **macOS**: download the `.dmg` (or `.tar.gz`), drag/run `jlocal`. Unsigned →
  right-click → *Open* on first launch (Gatekeeper). Apple Silicon build;
  Intel on request.
- **Linux**: download `jlocal-<ver>-x86_64-unknown-linux-gnu.tar.gz`,
  `tar xzf … && ./jlocal-…/jlocal`. No Flatpak/AppImage — plain tarball.

No admin rights needed: it only binds `127.0.0.1`.

## Design notes

- The juntos.lol server only hands `{relayUrl, path, publishToken}` + a `live`
  flag — media never touches the VPS. jlocal reuses that: the web UI keeps
  auth/room/`screenLive` signaling; jlocal only dials the relay with the same
  path. Browser capture (`getDisplayMedia`, no constraints, single
  opportunistic Opus track) stays the fallback; jlocal is the upsell for
  4K60 + system/per-app audio + persistence beyond the tab.
- Web integration points (all in juntos.lol, none here): `J` status pill in
  `Home` header + `Room` header (loopback probe à la `probeOne`, 1.5s
  timeout, 5–10s poll), download modal (`Dialog` + per-OS steps), screen
  overlay upsell banner, quality/audio-route controls posting to loopback.
- Tradeoffs accepted: fixed port (no port-hop, web allowlist stays valid);
  unsigned binaries (documented Gatekeeper/SmartScreen flow).
