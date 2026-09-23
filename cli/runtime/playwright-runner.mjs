// One invocation attaches to the browser already owned by the native daemon.
// This is ordinary Node/Playwright code in the workspace's existing trust
// boundary, not a hostile-code sandbox. Never launch or install a browser here.
import { Console } from 'node:console';
import { pathToFileURL } from 'node:url';
import { readFileSync, writeFileSync, closeSync } from 'node:fs';
import { createRequire } from 'node:module';

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
  const program = new AsyncFunction('page', 'context', 'browser', 'semanticJudgement', request.code);
  let semanticJudgement;
  if (request.environment?.semanticJudgementConfigPath) {
    const environment = JSON.parse(readFileSync(request.environment.semanticJudgementConfigPath, 'utf8'));
    const { createAmbitSemanticJudgementClient } = createRequire(import.meta.url)(request.environment.semanticJudgementClientModulePath);
    semanticJudgement = createAmbitSemanticJudgementClient(Object.freeze(environment));
  }
  const modulePath = process.env.AGENT_BROWSER_PLAYWRIGHT_MODULE;
  const { chromium, ambitCdpContextAdoptionVersion } = await import(modulePath ? pathToFileURL(modulePath).href : 'playwright-core');
  // Stock Playwright 1.62.1 folds every existing Chromium context into the
  // default one. With an isolated window open that would misreport cookies,
  // pages and context identity, so only the adoption build may attach then.
  if (ambitCdpContextAdoptionVersion !== 1 && request.isolatedContexts > 0) {
    throw new Error(`The installed Playwright client cannot represent ${request.isolatedContexts} open isolated window context(s) faithfully. Close those isolated windows or use native browser tools for them.`);
  }
  browser = await chromium.connectOverCDP(request.endpoint, {
    noDefaults: true,
    isLocal: true,
    artifactsDir: request.artifactsDir ?? undefined,
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
  // The daemon's proof that program code may have run. Written before the
  // program is invoked; a stop without this record performed no program work.
  writeFileSync(3, '{"started":true}\n');
  const value = await program(selected.page, selected.context, browser, semanticJudgement);
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
writeFileSync(3, `${JSON.stringify(result)}\n`);
closeSync(3);
