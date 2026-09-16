# Live Streaming

Stream a session's viewport over WebSocket and drive it with remote input. This is what a remote preview or embedded dashboard connects to: the browser runs wherever the daemon runs (a sandbox, a container, a CI box), and the client renders frames and sends clicks back.

**Related**: [commands.md](commands.md) for full command reference, [SKILL.md](../SKILL.md) for quick start.

## Contents

- [Enabling the stream](#enabling-the-stream)
- [Owned Chromium window](#owned-chromium-window)
- [Connecting](#connecting)
- [Messages from the server](#messages-from-the-server)
- [Messages from the client](#messages-from-the-client)
- [Frame rate and staleness](#frame-rate-and-staleness)
- [Limitations](#limitations)

## Owned Chromium window

While a primary presenter is connected, native capture and its delivery are capped at 20 fps. Secondary viewers stay capped at 10 fps, and capture returns to 10 fps after primary disconnect. Lower per-client limits apply to delivery; capture cadence follows primary presence. Existing acknowledgment pacing and latest-frame replacement bound work for a slow viewer; the raster limit is not a promise of constant frame rate.

`AGENT_BROWSER_WINDOW_STREAM=1` selects a private authenticated Linux Xvfb display for the same locally launched Chromium process used by CLI/MCP automation. Its frame includes native tabs, the address bar, menus, dialogs and the XFixes cursor. The host supplies a `browser-display` executable beside the native driver, or an absolute `AGENT_BROWSER_DISPLAY_HELPER` path. The helper and private display share the Chrome process lifetime. This mode refuses an inherited display; it does not attach to a global desktop.

The existing WebSocket upgrade accepts the `X-Ambit-Browser-Viewer` UUID header and `width`/`height` query parameters, all present or all omitted. Dimensions are positive CSS integers up to 2048. The first configured viewer owns layout; other viewers receive a secondary role and scale the same surface. Reconnect under the same UUID within two seconds to retain presentation ownership. The old connection's closure cannot clear a newer connection. A human input lease defers passive layout changes.

The existing stream sends these records:

```json
{"type":"presentation","role":"primary","requested":{"width":780,"height":600},"applied":{"kind":"browser-window","coordinateSpace":"display-pixels","generation":"d65bab6a-3a6d-42d5-826a-d794155ea2b6","width":1560,"height":1200,"originX":0,"originY":0,"deviceScaleFactor":2,"cursorIncluded":true}}
```

Each full-window `frame` has the same `surface` shape, its existing `seq`, and JPEG `data`; it omits page screencast `metadata`. The raster is bounded at 4096×4096. Native UI scale remains 2 independently of the viewing device and of Chromium's page zoom. The driver serializes actual RandR/window resize, clears prior page-metrics emulation, invalidates observations, and waits for native acknowledgment plus an unblocked visible-page compositor readback before publishing the applied surface. For a known native modal, it preserves the dialog and publishes the acknowledged native surface while page feedback remains unavailable. A refused layout reports `presentation.error: "viewport_unavailable"`. Large surfaces cost more to capture; the dimension limit is not a frame-rate guarantee.

Human control remains an internal host operation under the existing controller lease and input sequence. Full-window input adds the painted `expectedSurfaceGeneration`; mismatch returns `browser_control_surface_stale` without effects or sequence consumption. Mouse coordinates are physical display pixels, and wheel distances remain CSS distances. A `viewport` event accepts CSS dimensions and must be the sole event in the batch. The X11 helper supports keyboard and mouse; clients implement touch gestures through those native primitives instead of sending CDP touch coordinates over a window frame.

Copy invokes Chromium's native Copy on the current focused control, including browser chrome, and returns at most 1 MiB of exact UTF-8 text. Empty/password selections do not replace the user's local clipboard. Paste is one `input_keyboard` / `insertText` event per explicit intent. This single-event request permits 1 MiB of UTF-8 text, with an encoded native envelope bounded at `6 * 1024 * 1024 + 8192` bytes. Ordinary input envelopes remain at 65536 bytes. Both limits include native framing. Never split one paste into multiple native Ctrl+V operations or replay an unknown input. Clipboard transfer acknowledgment proves delivery, not completion of every application reaction.

Owned-window mouse commands move the captured native cursor and send buttons through the same display input owner as human control. The ordinary pre-hover supplies a measured page-to-window position, including page zoom and native browser chrome. A position lasts for one command or held gesture and is discarded on takeover. Native input is acknowledged by the display helper; no CDP button is replayed. Native motion can produce additional pointermove events, and Chromium may coalesce moves during a drag. A failed gesture or host timeout releases held native input through an acknowledged helper reset. If release is unconfirmed, new mouse input stays blocked. Cleanup never turns the original partial or unknown action into success; that classification and the fresh-observation requirement remain in the existing host response. Check and uncheck select one activation method before acting. Visible controls and associated labels use pointer input. A non-interactable associated input with no visible activation path uses the existing DOM control action, honors disabled state, and verifies the result. Responses report the actual method (`native`, `cdp`, `dom`, or `unchanged`); no action retries through another method.

Acknowledged agent input can carry a display-pixel action marker with `source: "agent"` and `surfaceGeneration`. The captured native cursor moves with the action. The marker describes dispatched input, not whether a website accepted its effect. Page-only CDP sessions keep their existing input path. Neither telemetry path exposes typed text or clipboard content.

Before ordinary page actions, the driver observes native window focus and actual page visibility. Ambiguous focus and pinned-tab mismatches require explicit tab selection. After human handback or a layout change, CLI callers must obtain a new snapshot or screenshot; host-bound MCP supplies fresh feedback with `browser_observation_required`. Queued actions admitted before the layout change cannot use a later observation to replay stale coordinates. Native window closure leaves the existing daemon available for an explicit `open`, which creates a new browser with a fresh target. `close` ends that session and reaps its display. Unlabelled stream loss still does not prove permanent closure.

## Enabling the stream

Streaming is always available; the server binds an OS-assigned localhost port unless told otherwise.

```bash
agent-browser stream status --json     # Report enabled state, port, client count
agent-browser stream enable            # Create the server (--port to pin one)
agent-browser stream disable           # Tear it down
```

`AGENT_BROWSER_STREAM_PORT` pins the port for the whole daemon instead of passing `--port`.

Repeated `stream enable` calls keep the current stream and its connected viewers. Omitting `--port`, using `--port 0`, or specifying the active port returns its current status. A different explicit port is refused until you disable the stream. Repeated `stream disable` calls succeed without restarting or closing the browser.

Frame encoding is daemon-wide, read once at startup:

| Variable | Default | Notes |
|---|---|---|
| `AGENT_BROWSER_STREAM_QUALITY` | `80` | 0 to 100, clamped |
| `AGENT_BROWSER_STREAM_MAX_WIDTH` | the viewport | caps the frame, does not resize the page |
| `AGENT_BROWSER_STREAM_MAX_HEIGHT` | the viewport | same |

The live stream requests jpeg, since a `frame` message carries no format field. An explicit `screencast_start` reconfigures the same underlying screencast, so a client can still see the format change mid-stream; sniff the bytes rather than assuming. Measured on a busy page at 1280x720: quality 80 gives ~54 KB per frame, quality 20 gives ~25 KB, and quality 20 at 640x360 gives ~9 KB. An unusable value leaves the default.

Read the port from `stream status --json` rather than assuming one; the OS-assigned default changes per daemon.

## Connecting

Connect a WebSocket client to `ws://127.0.0.1:<port>`. Frame delivery starts automatically once a client attaches, so there is no subscribe message. Browser clients must load from `localhost`, `127.0.0.1`, `::1` or `file://`. Any other origin gets a 403 on the upgrade and needs a proxy.

## Messages from the server

Every message is JSON text with a `type` field.

- `frame`: a viewport image plus its metadata. Delivered latest-first (see below).

```json
{
  "type": "frame",
  "seq": 41,
  "data": "<base64-encoded-jpeg>",
  "metadata": {
    "deviceWidth": 1280, "deviceHeight": 720, "pageScaleFactor": 1,
    "offsetTop": 0, "scrollOffsetX": 0, "scrollOffsetY": 0,
    "timestamp": 1785038682238
  }
}
```

`seq` is a monotonic frame id, echoed back under ack pacing and stable across browser relaunches. `metadata.timestamp` is the capture time in epoch milliseconds, so `Date.now() - timestamp` is the age of the frame being drawn. The other message types:

- `status`: connection state, screencasting flag, viewport size, engine, recording flag. Sent once on connect and again on change.
- `tabs`: the current tab list, sent on connect when tabs are known and on change.
- `url`: on Chrome, full-document, History API, and fragment navigation in the active tab's main frame. Child-frame and background-tab navigation is ignored.
- `console`: console events.
- `finished`: this stream ended intentionally through session closure or `stream disable`. It precedes orderly WebSocket closure for responsive viewers. An unlabelled disconnect is not equivalent and may be a transport failure. A new stream after reopening is a new instance.

Status, tabs, url, and console travel on an ordered channel: they are delivered in order and are never replaced by a newer message the way frames are. They are not unconditionally durable. A client that falls far enough behind can lag out of that channel and lose messages it never saw, so treat console output as a live feed, not an audit log.

## Messages from the client

```json
{"type": "input_mouse", "eventType": "mousePressed", "x": 40, "y": 40, "button": "left", "clickCount": 1}
{"type": "input_keyboard", "eventType": "keyDown", "key": "a", "text": "a"}
{"type": "input_touch", "eventType": "touchStart", "touchPoints": []}
{"type": "config", "maxFps": 10}
{"type": "config", "pacing": "ack"}
{"type": "ack", "seq": 41}
```

Input dispatches to the browser on a task of its own, separate from frame delivery, so a click is not queued behind a frame write. Events are sent to the browser without waiting for its reply, so a click stays responsive behind a burst of mouse moves. Ordering is preserved: press never overtakes move. Mouse, keyboard, and touch input also reset the daemon idle timer, so an actively driven preview is not shut down by the idle timeout.

`config` sets a per-client frame cap: 1 to 120, or `0` for uncapped (the default). It takes effect immediately, including when it loosens the cap. Each client's cap is its own; other connected clients are unaffected. A value above 120 is clamped to 120; a negative or non-numeric value is ignored, leaving the current cap in place. Neither rejects the connection.

Both settings can also be declared on the URL, which is the only way to have them cover the connection's opening frame: `ws://127.0.0.1:<port>/?pacing=ack&maxFps=10`. A `config` message sent after connecting still wins.

## Frame rate and staleness

The server holds only the newest frame per client and reads it at send time. A frame produced while an earlier one is still being written is skipped, not queued, so the application never builds a backlog.

Push pacing (the default) stops there, and the transport underneath is still ordered: frames already accepted by the socket are delivered in order, so a client that stalls drains whatever the kernel buffered before the writer blocked.

Ack pacing closes that gap. Send `{"type":"config","pacing":"ack"}` and the server keeps at most one frame in flight, waiting for `{"type":"ack","seq":N}` before sending the next. Every frame carries a monotonic `seq`; echo the one you finished rendering. Frames produced while an ack is outstanding replace each other and never reach the socket, so a client that stalls for ten seconds and resumes gets the current page, not ten seconds of history.

Under ack pacing one frame is in flight at a time, so the rate is one frame per transfer plus one acknowledgement round trip. Both the link's bandwidth and its latency bound it, and a link whose bandwidth-delay product exceeds a single frame goes underused. Ack pacing bounds one hop. With a proxy in the path, forward the renderer's acks; acks generated on receipt leave frames queued on the far side. Acks are cumulative, so acknowledging a newer id covers any older one. A client that opts in and then stops acking simply stops receiving frames; status, tabs, url, and console keep flowing.

The two settings compose: `pacing` bounds how much is in flight, `maxFps` bounds the rate. A constrained preview usually wants both.

## Limitations

- Localhost only. Exposing the stream beyond the machine is the embedder's job (tunnel, proxy, or port forward), and the origin allowlist applies to browser clients.
- Frames are images, not a video codec. Bandwidth scales with viewport size and page activity; cap the rate for constrained links.
- In push pacing the server cannot tell a slow renderer from a fast one beyond transport backpressure. Use ack pacing when that distinction matters.


## Human file transfer

The internal `ambit_browser_control` bridge advertises `filesSupported: true` from `inspect` only when this driver has an available Chromium browser. Clients must treat an omitted capability as false, preserving older relay and driver compatibility. `files` takes `controllerId` and returns `status: "files"`, `chooser` (null or an opaque `destinationId`, `kind`, `accept`, and `multiple`), and observed completed `downloads` begun since that controller acquired custody. Each download includes its GUID as `id` and `guid`, observed byte count, suggested filename, frame, and original absolute browser path. These paths are for the authorized Product capture adapter; never relay them to an untrusted viewer or treat them as laptop paths. Polling neither consumes download-command results nor accepts a requested path. History retains the original observation if a source file moves or disappears; Product capture/read, rather than metadata polling, proves the bytes when consumed. For manual export after handback or reconnection, read-only `downloads` takes no controller fields and returns `supported`, `controlled`, and all retained completed downloads for the owned browser, including downloads from closed tabs (external browser attachment remains scoped to the admitted page). Product resource authorization is still required; this read does not acquire custody.

`drop` takes the controller, next shared `sequence`, painted `expectedSurfaceGeneration`, and `x`/`y`; it returns `status: "destination"` and a destination for the actual element reached by that pointer. Window coordinates are physical display pixels. `setfiles` takes the controller, next sequence, `destinationId`, and 1 through 64 already staged canonical regular-file paths; no file bytes enter the input JSON. Choosers use `DOM.setFileInputFiles`. Website drops use Chromium's native drag dispatch with real files, current node geometry, exact hit tests and trusted event receipts. Navigation, renderer detachment, node replacement, release, and expiry refuse stale destinations. Page mutation between native dispatch and its observed event can still reject a drop; successful dispatch proves the browser event, not the website's later upload or server response. `dismissfiles` takes the controller and destination without advancing the sequence or clearing the input's existing selection. Directory pickers are explicitly unsupported. These operations remain private host integration, with no public CLI command or MCP tool; Product owns user authorization, laptop pickers, immutable staging, capture and local download delivery.
