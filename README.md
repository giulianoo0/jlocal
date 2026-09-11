# jlocal

Local companion for [juntos.lol](https://github.com/giulianoo0/juntos.lol): torrent locally (no worker),
share screen up to 4K60 via the MoQ relay, all through one tiny native app.

> Loopback API + status/version window. Nothing else.

The app itself has no web UI. Its window is a small status card (drawn
in-process with `softbuffer` + bundled Inter, no toolkit):

- **Site**: connected + loopback endpoint (also `/health`)
- **Gravação de tela**: OS capture consent, live probe (`/capabilities` → `permissions.screenCapture`)
- **Torrent**: whether the local engine booted
- **Atualização**: `vX.Y.Z disponível` / `em dia` / `verificando…`, with an
  *Instalar vX.Y.Z* button when one is known and *Verificar atualizações*
  otherwise; *Sair* quits.

It also lives in the menubar tray (monotone icon): closing the window hides
to the tray instead of quitting — on macOS the app also leaves the Dock until
opened again — and the tray menu has *Abrir jlocal*, *Verificar
atualizações*, *Instalar atualização vX* (when one is known) and *Sair do
jlocal*. The version is also in `/version` and `--version`.

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
| `GET /capabilities` | `{name, version, capabilities}` (`screen.capture`: frame capture live; `screen.available`: relay publish, still `false`; `audio.capture`: per-app system-audio tap wired (macOS/Windows) vs `false` (Linux stub); audio/torrent flags; `permissions.screenCapture`: OS capture consent, live probe) |
| `GET /audio/apps`   | `{apps:[{id,name}]}` when listable, else `501 {error}`           |
| `POST /audio/mode`  | `{mode}` persists `all`/`none`/`custom` (mute set preserved, takes effect live) |
| `POST /audio/mute`  | `{app, muted}` persists one app toggle (takes effect live)       |
| `GET /audio/stream` | System-audio mix while a capture session is live: infinite raw `s16le` 48 kHz stereo PCM (`Content-Type: audio/L16`), one 3840-byte chunk per ~20 ms tick, chunked + `no-store` + CORS; `404 {error:"idle"}` when stopped (the body also ends on `/capture/stop`); `501 {error}` on Linux. Mute semantics: `all` = full mix, `none` = silence (stream stays open), `custom` = mix minus muted apps (`app` ids match `/capture/windows`). macOS taps via ScreenCaptureKit (single system mix + `excludingApplications` filter for mutes; needs Screen Recording); Windows via per-process WASAPI loopback taps (one client per audible process, muted processes dropped). Underflow or a refused tap degrades to silence, never a hang. |
| `GET /capture/windows` | `{windows:[{id,name,app,icon,width,height}]}` from the OS (minimized/zero-area skipped; `icon` is the owning app's icon as a `data:image/png;base64,…` URL at 32px, `""` when unavailable — macOS + Windows only, Linux always `""`) |
| `GET /capture/preview.jpg` | Latest frame JPEG, `404 {error:"idle"}` when stopped     |
| `GET /capture/snapshot` | One-frame JPEG for picker previews: `?display_id=<id\|empty=primary>` xor `?window_id=<id>`, `&width=<px>` (default 960, clamp 160–1920, aspect kept); `200 image/jpeg`; `400` both/neither/garbage/unknown id; `503 {error:"permission"\|"unavailable"}` (stateless — never touches the running session) |
| `GET /capture/stream` | Live MJPEG (`multipart/x-mixed-replace;boundary=frame`) of the running session at its fps; ends on stop; `404 {error:"idle"}` when stopped |
| `GET /capture/h264` | macOS: hardware H.264 (ScreenCaptureKit → VideoToolbox) of the running H.264 session as an infinite byte stream: per frame a big-endian `u32` payload length, `u64` presentation time in µs, one flag byte (bit 0 = keyframe), then the Annex-B access unit (SPS/PPS in-band on keyframes). Opens on a keyframe; `404 {error:"idle"}` when no H.264 session runs. The web injects these straight into its MoQ publisher, no re-encode. |
| `POST /capture/start` | `{display_id _xor_ window_id,width,height,fps}` (+ `codec:"h264"` and optional `bitrate` on macOS: starts the hardware session for `GET /capture/h264` instead, 60 fps real; the JPEG session keeps serving previews) → `{started,display_id\|window_id,width,height,fps}` (exactly one id; unknown id is `400`; size is encode quality up to 3840x2160 — larger than the target upscales Discord-style; grabs one frame synchronously so an uncapturable target fails here instead of publishing black; a window closed mid-session ends frames so preview `404`s) |
| `POST /capture/stop`  | `{stopped:true}` (idempotent, always 200)                      |
| `POST /torrent/add` | `{id, name}` (waits ≤60s for magnet metadata; `504` on timeout)  |
| `GET /torrent/list` | `{torrents:[{id,name,size,progress,state,downBps,files}]}`        |
| `GET /torrent/data/{id}/{file}` | `206` ranged bytes (`Range`, index or path; `416` + `Content-Range: bytes */total` when unsatisfiable; single reads capped at 64 MiB) |
| `POST /torrent/select` | `{selected:true}` (focuses the swarm on one file)             |
| `GET /torrent/stats/{id}` | `{peers,downBps,downloaded,progress}`                       |
| `DELETE /torrent/{id}` | `{removed:true}` (refuses metadata-less torrents: retry later) |
| `GET /youtube/tools` | `{status}`: `ready`, `missing`, `downloading` (+`done`,`total` bytes), `failed` (+`error`) or `unsupported`. yt-dlp and FFmpeg are not in the bundle: they are fetched once into `~/Library/Application Support/jlocal/tools` (macOS) or `%LOCALAPPDATA%\jlocal\tools` (Windows), pinned by sha256 |
| `POST /youtube/tools` | starts the download; `202` with the same body, `501` where no tools are pinned |
| `POST /youtube/resolve` | `{url}` → `{summary}` (title, duration, chosen video/audio/subtitle tracks, no CDN urls) or `502 {error, detail}` with the site's codes (`youtube_blocked`, `youtube_unavailable`, `youtube_unsupported`, `youtube_tool`); `503 {error:"tools_missing"}` before the download |
| `POST /youtube/run` | `{url, runId, claim, roomId, mediaGeneration, region, startMs, apiBase}` → `202 {runId}`: prepares the video with the fleet's `ss-remux` crate and publishes into the room through `apiBase` under `claim`; a new run for the same room supersedes the previous one (that is how a seek arrives) |
| `GET /youtube/run/{id}` | `{runId, state, producedMs, error}`; `404 {error:"unknown_run"}` |
| `DELETE /youtube/run/{id}` | `{cancelled}` |

Security: `Host` must be loopback (DNS-rebinding guard); CORS echoes
allowlisted origins (`Access-Control-Allow-Origin` + Vary) on every endpoint
including `GET /health`; `Access-Control-Allow-Private-Network: true`;
`Cache-Control: no-store`. Browsers should `fetch` (not `EventSource`).

Torrent bytes download into the OS temp dir (`jlocal-torrents`); session
state persists there for fastresume. No UPnP: the app never punches NAT holes.
Reads ahead of the download block on the swarm (no prefetch beyond the
engine's lookahead) and fail after 90s per read (`504`).

Screen publish is still unwired, but no longer for lack of a transport: the
relay speaks drafts 14 and 16, and the Rust side of the same monorepo the
web publisher comes from (`moq-net` pinned to `Ietf(Draft16)`, `moq-native`,
`hang`) speaks draft-16. What is missing is the native encoder pipeline
(hardware H.264 up to 4K60 + Opus) and the wiring; `src/publish.rs` pins the
wire contract (`juntos/<room>/<secret>/<member>.hang`, hang catalog, `legacy`
container). Until then `screen.available` stays `false` and the web UI keeps
routing to the browser picker.

Env: `JLOCAL_PORT` (default `40392`), `JLOCAL_NO_UI`, `JLOCAL_ALLOWED_ORIGINS`
(comma-separated `https://` origins, replaces the juntos.lol + beta defaults),
`JLOCAL_VERSION` (injected by release CI).

## Self-update

On boot and every 6h the app `GET`s
`https://github.com/giulianoo0/jlocal/releases/latest` without following
redirects (10s timeout, best-effort, never blocks boot) and reads the newest
tag off the 302's `location` (`…/releases/tag/vX.Y.Z`). When the tag is newer than its own build
it shows `vX disponível` in the window (with an *Instalar vX* button) and
enables the tray's *Instalar atualização vX* item (plus a manual
*Verificar atualizações* item).
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
