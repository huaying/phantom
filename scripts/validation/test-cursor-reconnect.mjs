// Local transport_smoke fixture only. Node 22+ and an isolated Chrome instance.
// Each fixture session advertises a distinct cursor only after both opt-ins.
import fs from 'node:fs/promises';

const [cdpPort = '9272', fixturePort = '9921', output] = process.argv.slice(2);
if (!/^\d+$/.test(cdpPort) || !/^\d+$/.test(fixturePort) || !output) {
  throw Error('Usage: test-cursor-reconnect.mjs CDP-port fixture-port output-dir');
}
await fs.mkdir(output, {recursive: true});
const base = `http://127.0.0.1:${cdpPort}`;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const results = [];

// The fixture serves one session at a time. Run RTC last because closing its
// final tab can leave consent expiry pending for up to 35 seconds.
for (const mode of ['wss', 'rtc']) {
  const page = await fetch(`${base}/json/new?about:blank`, {method: 'PUT'}).then(r => r.json());
  const socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let sequence = 0;
  const pending = new Map(), errors = [], steps = [];
  socket.onmessage = event => {
    const message = JSON.parse(event.data), request = pending.get(message.id);
    if (message.method === 'Runtime.exceptionThrown') errors.push(message.params.exceptionDetails);
    if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error') errors.push(message.params.args);
    if (request) {
      pending.delete(message.id);
      clearTimeout(request.timer);
      message.error ? request.reject(Error(JSON.stringify(message.error))) : request.resolve(message.result);
    }
  };
  const call = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(Error(`${method} timeout`)); }, 15000);
    pending.set(id, {resolve, reject, timer});
    socket.send(JSON.stringify({id, method, params}));
  });
  const evaluate = async expression => {
    const result = await call('Runtime.evaluate', {expression, returnByValue: true});
    if (result.exceptionDetails) throw Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  const snapshot = () => evaluate(`({debug:window.__phantom_debug,pcState:window.__phantom_pc?.connectionState,sockets:window.__cursorSockets?.map(w=>({path:new URL(w.url).pathname,state:w.readyState}))})`);
  const until = async expression => {
    const started = Date.now();
    while (Date.now() - started < 20000) {
      if (await evaluate(expression)) return;
      await sleep(100);
    }
    throw Error(`Cursor subscription timeout: ${JSON.stringify(await snapshot())}`);
  };
  try {
    await call('Page.enable');
    await call('Runtime.enable');
    await call('Page.addScriptToEvaluateOnNewDocument', {source: `
      window.__cursorSockets=[];
      const NativeSocket=window.WebSocket;
      window.WebSocket=class extends NativeSocket {
        constructor(...args){super(...args);window.__cursorSockets.push(this);}
      };
    `});
    await call('Page.navigate', {url: `https://127.0.0.1:${fixturePort}/?${mode}`});
    await until('window.__phantom_debug?.cursorAppliedShapeId>0');
    for (let cycle = 0; cycle < 4; cycle++) {
      if (cycle) {
        const previous = steps.at(-1).debug.cursorAppliedShapeId;
        if (mode === 'rtc') await evaluate('window.__phantom_pc.close()');
        else await evaluate(`__cursorSockets.find(w=>w.readyState===1&&new URL(w.url).pathname==='/ws').close(1000,'cursor regression')`);
        await until(`window.__phantom_debug?.cursorAppliedShapeId>0 && window.__phantom_debug.cursorAppliedShapeId!==${previous}`);
      }
      const state = await snapshot();
      const passed = state.debug.cursorShapeId === state.debug.cursorAppliedShapeId
        && state.debug.cursorShapeCount >= cycle + 1
        && (mode !== 'rtc' || state.pcState === 'connected') && errors.length === 0;
      steps.push({cycle, passed, ...state});
      console.log(JSON.stringify({mode, cycle, passed, shape: state.debug.cursorAppliedShapeId}));
      if (!passed) throw Error('Cursor state or bitmap not applied');
    }
    results.push({mode, passed: true, steps, errors});
  } catch (error) {
    results.push({mode, passed: false, error: String(error), steps, errors});
    process.exitCode = 1;
  } finally {
    socket.close();
    await fetch(`${base}/json/close/${page.id}`);
    await fs.writeFile(`${output}/cursor-reconnect.json`, JSON.stringify(results, null, 2));
  }
}
