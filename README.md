# jlocal

Local companion for [juntos.lol](https://github.com/giulianoo0/juntos.lol): torrent locally (no worker),
share screen up to 4K60 via the MoQ relay, all through one tiny native app.

> Loopback API + status/version window. Nothing else.

The app itself has no web UI. It shows two things:

- **status**: connected / not connected (window title + console + `/health`)
- **version**: the release tag (window title + `/version` + `--version`)

Everything else (preview, resolution/fps picker, per-app audio toggles) lives
in the juntos.lol web UI, which talks to this app over loopback.

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

| Endpoint      | Description                                                      |
|---------------|------------------------------------------------------------------|
| `GET /health` | `{name, version, connected, moq, torrent, port}`                  |
| `GET /version`| `{name, version}`                                                |
| `GET /events` | SSE: `hello` + 15s heartbeats.                                   |

Security: `Host` must be loopback (DNS-rebinding guard); CORS echoes
allowlisted origins (`Access-Control-Allow-Origin` + Vary) on every endpoint
including `GET /health`; `Access-Control-Allow-Private-Network: true`;
`Cache-Control: no-store`. Browsers should `fetch` (not `EventSource`).

Env: `JLOCAL_PORT` (default `40392`), `JLOCAL_NO_UI`, `JLOCAL_ALLOWED_ORIGINS`
(comma-separated `https://` origins, replaces the juntos.lol + beta defaults),
`JLOCAL_VERSION` (injected by release CI).

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
