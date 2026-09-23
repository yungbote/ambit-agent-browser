# Shared Playwright execution

Ordinary `window new` shares the persistent profile. `window new --isolated` retains a separate native cookie context. Playwright 1.62.1 cannot faithfully represent pre-existing isolated contexts and attachment is refused while those windows are open, including when the selected tab belongs to the persistent profile. Use native tools for those windows, or close them before returning to Playwright. Existing authentication state is never copied or migrated.

Linux private window streaming defaults to ANGLE software GLES for WebGL. Explicit browser arguments override the preset; `--webgpu` retains its own backend. Fresh driver-owned profiles start on `about:blank`; retained profiles and custom startup arguments are unchanged. The owned virtual display uses X11 independently of an inherited host Wayland session.

The Unix native daemon runs one supervised Node program against its existing Chromium instance. The host installs Node and a qualified `playwright-core` version (1.62.1 supports the required `noDefaults` attachment). The runner never installs packages or browsers. `AGENT_BROWSER_PLAYWRIGHT_MODULE` selects an absolute installed `playwright-core/index.mjs`; without it normal Node package resolution applies. `AGENT_BROWSER_NODE_PATH` selects Node. `AGENT_BROWSER_PLAYWRIGHT_RUNNER` optionally selects the installed runner module; otherwise the native binary uses its bundled runner.

```sh
agent-browser open https://example.test
agent-browser run-playwright --timeout-ms 30000 --stdin <<'JS'
await page.getByRole('textbox', { name: 'Search' }).fill('reference');
return await page.getByRole('link').allTextContents();
JS
```

The code is an async function body with the actual Playwright `page`, `context`, and `browser` objects. The default page is the driver's current target. `--target <id>` selects an exact existing CDP target. MCP exposes `{code, targetId?, timeoutMs?}` as `agent_browser_run_playwright` in the core and host-bound profiles. Ownership and launch settings remain host-selected in host-bound mode.

Use normal Playwright locators, frame locators, events, uploads, screenshots and asynchronous composition. Existing tabs and cookies stay in the native browser. New contexts are ordinary separate Playwright contexts and do not inherit the authenticated persistent context. Attachment uses `noDefaults: true` so native focus, media and download configuration are retained. Screenshots and downloads should use the host's existing workspace artifact paths. CDP attachment has Playwright's documented compatibility limits.

The temporary transport routes actual mouse input through the native display owner in window mode. It does not interpolate movement, delay actions for animation or manufacture pointer activity for DOM evaluation. Pointer, click and scroll activity is published to viewers of the native page exactly as native commands publish it; `element.click()` in page script publishes none. Keyboard text and ordinary key events use the native owner; browser editing commands and composition retain CDP semantics. Touch remains CDP input. In the owned window the mouse wheel is a real device: `mouse.wheel` deltas are sent as notches of 100 units and a remainder carries into the next wheel call. No new viewer protocol or public debugging URL is created.

Return a JSON-serializable value (maximum 2 MiB). `undefined` becomes `null`. Console output is returned as diagnostic text, capped at 64 KiB with an explicit truncation flag. Code is limited to 1 MiB and the operation deadline is 1 through 120000 milliseconds, defaulting to 30000. The native owner settles processes and input after the deadline before releasing custody.

Human takeover, caller disconnect, timeout and native shutdown stop the runner's process group and detach its temporary connection while retaining Chrome. The current action is not rolled back. Program code runs only after the runner has attached and recorded its start on a private channel. A stop before that record proves nothing ran: human control reports `browser_controlled_by_user`, and a deadline, shutdown or runner failure reports `browser_operation_rejected`. After program execution starts, an exception, serialization failure, deadline or lost result is `browser_operation_outcome_unknown`. A verified program stop requested for human control is `browser_operation_interrupted`, with `executionStopped: true` and `effectsMayHaveOccurred: true`; this does not itself prove that human acquisition succeeded. The host verifies its current controller before opening its existing handback wait. Inspect current browser and external state before choosing another action; never automatically replay the body. Deliberately issued browser/page commands retain their real effects.

This execution uses the workspace's existing trust boundary. Same-user arbitrary workspace code already has access to local process and browser resources; the temporary transport and hidden endpoint are not a hostile-code sandbox or an additional tenant boundary.
