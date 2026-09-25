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

// The program's stack frames carry this name, so a failure is located in the
// program's own lines. A dynamic function's body starts on the third line of
// its source (ECMA-262 CreateDynamicFunction).
const PROGRAM_URL = 'agent-browser-program.js';
const PROGRAM_FRAME = /^\s+at .*\bagent-browser-program\.js:(\d+):(\d+)\)?$/m;
const BODY_LINE_OFFSET = 2;
const MAX_FAILURE_TEXT = 16_384;

const record = await execute();
// Keep the group leader owned until the daemon has killed the entire operation
// group and reaped it. Background timers or child processes cannot extend a
// successful program into the next owner's turn.
setInterval(() => {}, 60_000);
writeFileSync(3, `${JSON.stringify(record)}\n`);
closeSync(3);

/**
 * The result record: the program's value, the runner's refusal before any
 * program code (`error`), or the program's own failure (`program`), which
 * says how many Playwright calls it issued before failing.
 */
async function execute() {
  let program;
  try {
    // Compile before attachment so syntax errors cannot perform browser work.
    program = new AsyncFunction('page', 'context', 'browser', 'semanticJudgement', `${request.code}\n//# sourceURL=${PROGRAM_URL}`);
  } catch (error) {
    return programFailure(error, { issued: 0 });
  }
  let browser;
  let selected;
  let semanticJudgement;
  let calls;
  try {
    if (request.environment?.semanticJudgementConfigPath) {
      // The host-written relay map holds this Action's credentials. JSON and
      // loader errors can quote their input, so none of them reach the result.
      try {
        const environment = JSON.parse(readFileSync(request.environment.semanticJudgementConfigPath, 'utf8'));
        const { createAmbitSemanticJudgementClient } = createRequire(import.meta.url)(request.environment.semanticJudgementClientModulePath);
        semanticJudgement = createAmbitSemanticJudgementClient(Object.freeze(environment));
      } catch {
        throw new Error('The semantic judgement binding for this program is unavailable.');
      }
    }
    const modulePath = process.env.AGENT_BROWSER_PLAYWRIGHT_MODULE;
    const { chromium, ambitCdpContextAdoptionVersion } = await import(modulePath ? pathToFileURL(modulePath).href : 'playwright-core');
    // Stock Playwright 1.62.1 folds every existing Chromium context into the
    // default one. With an isolated window open that would misreport cookies,
    // pages and context identity, so only the adoption build may attach then.
    if (ambitCdpContextAdoptionVersion !== 1 && request.isolatedContexts > 0) {
      throw new Error(`The installed Playwright client cannot represent ${request.isolatedContexts} open isolated window context(s) faithfully. Close those isolated windows or use native browser tools for them.`);
    }
    // The daemon's deadline is the only clock: it stops this process group and
    // then names what kept the attachment from completing. A second timer here
    // would race it with a less specific error.
    browser = await chromium.connectOverCDP(request.endpoint, {
      noDefaults: true,
      isLocal: true,
      artifactsDir: request.artifactsDir ?? undefined,
      timeout: 0,
    });
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
    calls = observeCalls(browser);
  } catch (error) {
    if (browser) await closeAttachment(browser, null);
    return { success: false, error: failureText(error instanceof Error ? error.message : String(error)) };
  }
  // The daemon's proof that program code may have run. Written before the
  // program is invoked; a stop without this record performed no program work.
  writeFileSync(3, '{"started":true}\n');
  let outcome;
  try {
    const value = await program(selected.page, selected.context, browser, semanticJudgement);
    outcome = { success: true, result: value === undefined ? null : value };
    // Reject cyclic values/BigInt before declaring the program complete.
    JSON.stringify(outcome);
  } catch (error) {
    outcome = { success: false, thrown: error };
  }
  // A call the program left running is counted until the attachment is
  // closed; a count taken while it could still be open proves nothing.
  const closed = await closeAttachment(browser, calls);
  return outcome.success ? outcome : programFailure(outcome.thrown, closed ? calls : null);
}

/**
 * Counts the Playwright calls issued once the program starts: every message
 * the client hands its in-process server, whichever object carried it (page,
 * context, browser, or a locator, frame, handle, keyboard, request context or
 * event argument reached from them). `__waitInfo__` is wait metadata for
 * Playwright's own logs, which its server never acts on, so it is not a call.
 * This path is the qualified client's internals; a client without it yields
 * null, and no count is claimed for its programs.
 */
function observeCalls(browser) {
  const connection = browser._connection;
  const send = connection?.sendMessageToServer;
  if (typeof send !== 'function') return null;
  const calls = { issued: 0, last: null, closing: false };
  connection.sendMessageToServer = function (object, method, params, options) {
    const own = calls.closing && object === browser && method === 'close';
    if (!own && method !== '__waitInfo__') {
      calls.issued++;
      calls.last = options?.apiName || `${object._type}.${method}`;
    }
    return send.call(this, object, method, params, options);
  };
  return calls;
}

/**
 * Playwright's CDP attachment close disconnects its transport; the native
 * daemon remains the owner of Chrome, the profile and any retained tabs. The
 * runner's own close is not a program call. Playwright issues its message
 * synchronously, so exactly that message is left uncounted: anything else
 * sent meanwhile, such as a program's wrapper around `browser.close`, still
 * counts. Resolves to whether the attachment closed.
 */
function closeAttachment(browser, calls) {
  if (calls) calls.closing = true;
  const closing = browser.close();
  if (calls) calls.closing = false;
  return closing.then(() => true, () => false);
}

/** The program's own failure, and how far it reached into the browser. */
function programFailure(thrown, calls) {
  return {
    success: false,
    program: {
      error: describeThrown(thrown),
      pageCallsIssued: calls ? calls.issued : null,
      ...(calls?.issued ? { lastPageCall: calls.last } : {}),
    },
  };
}

/** What the program threw, located in its own lines when its stack names them. */
function describeThrown(thrown) {
  if (!(thrown instanceof Error)) return { message: failureText(String(thrown)) };
  const described = { name: failureText(thrown.name), message: failureText(thrown.message) };
  const frame = PROGRAM_FRAME.exec(String(thrown.stack));
  if (frame && Number(frame[1]) > BODY_LINE_OFFSET) {
    described.line = Number(frame[1]) - BODY_LINE_OFFSET;
    described.column = Number(frame[2]);
  }
  return described;
}

/**
 * Failure text reaches the model: the private endpoint is never quoted,
 * terminal colour codes are dropped, and a failure always fits its record.
 */
function failureText(text) {
  const plain = String(text).split(request.endpoint).join('[browser connection]').replace(/\u001b\[[\d;]*m/g, '');
  return plain.length > MAX_FAILURE_TEXT ? `${plain.slice(0, MAX_FAILURE_TEXT)}…` : plain;
}
