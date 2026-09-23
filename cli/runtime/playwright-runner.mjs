// One invocation attaches to the browser already owned by the native daemon.
// This is ordinary Node/Playwright code in the workspace's existing trust
// boundary, not a hostile-code sandbox. Never launch or install a browser here.
import { Console } from 'node:console';
import { pathToFileURL } from 'node:url';

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString('utf8'));
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const diagnostics = new Console({ stdout: process.stderr, stderr: process.stderr });
globalThis.console = diagnostics;

let browser;
let started = false;
let result;
try {
  // Compile before attachment so syntax errors cannot perform browser work.
  const program = new AsyncFunction('page', 'context', 'browser', request.code);
  const modulePath = process.env.AGENT_BROWSER_PLAYWRIGHT_MODULE;
  const { chromium } = await import(modulePath ? pathToFileURL(modulePath).href : 'playwright-core');
  browser = await chromium.connectOverCDP(request.endpoint, {
    noDefaults: true,
    timeout: request.timeoutMs,
  });
  let selected;
  for (const context of browser.contexts()) {
    for (const page of context.pages()) {
      const session = await context.newCDPSession(page);
      try {
        const { targetInfo } = await session.send('Target.getTargetInfo');
        if (targetInfo.targetId === request.targetId) selected = { page, context };
      } finally {
        await session.detach();
      }
    }
  }
  if (!selected) throw new Error('The selected browser tab is no longer available.');
  started = true;
  const value = await program(selected.page, selected.context, browser);
  result = { success: true, result: value === undefined ? null : value };
  // Reject cyclic values/BigInt before declaring the program complete.
  JSON.stringify(result);
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  result = { success: false, started, error: message.split(request.endpoint).join('[browser connection]') };
} finally {
  // Playwright's CDP attachment close disconnects its transport; the native
  // daemon remains the owner of Chrome, the profile and any retained tabs.
  if (browser) await browser.close().catch(() => {});
}
// Keep the group leader owned until the daemon has killed the entire operation
// group and reaped it. Background timers or child processes cannot extend a
// successful program into the next owner's turn.
setInterval(() => {}, 60_000);
process.stdout.end(`${JSON.stringify(result)}\n`);
