// Node 22+, Chrome with --remote-debugging-port=9270, and transport_smoke.
// Uses its own tab and reads only the synthetic fixture's rendered pixels.
const [mode = 'rtc', debugPort = '9270', mediaPort = '9921'] = process.argv.slice(2);
if (!['rtc', 'wss'].includes(mode) || !/^\d+$/.test(debugPort) || !/^\d+$/.test(mediaPort)) {
  throw new Error('Usage: node probe_transport.mjs [rtc|wss] [CDP port] [fixture port]');
}
const debugUrl = `http://127.0.0.1:${debugPort}`;
const page = await fetch(`${debugUrl}/json/new?about:blank`, {method: 'PUT'}).then(r => r.json());
const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
let nextId = 0;
const pending = new Map();
socket.onmessage = event => {
  const message = JSON.parse(event.data);
  const entry = pending.get(message.id);
  if (!entry) return;
  pending.delete(message.id);
  clearTimeout(entry.timer);
  if (message.error) entry.reject(new Error(JSON.stringify(message.error)));
  else entry.resolve(message.result);
};
function call(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)); }, 10000);
    pending.set(id, {resolve, reject, timer});
    socket.send(JSON.stringify({id, method, params}));
  });
}
async function evaluate(expression) {
  const result = await call('Runtime.evaluate', {expression, returnByValue: true, awaitPromise: true});
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
  return result.result.value;
}
const sampleExpression = `(() => {
  const source = document.querySelector('video') || document.querySelector('canvas');
  const width = source?.videoWidth || source?.width;
  const height = source?.videoHeight || source?.height;
  if (width !== 640 || height !== 360) return null;
  const canvas = document.createElement('canvas');
  canvas.width = width; canvas.height = height;
  const context = canvas.getContext('2d', {willReadFrequently: true});
  context.drawImage(source, 0, 0);
  const pixels = context.getImageData(0, 20, 640, 1).data;
  let timestamp = 0;
  for (let bit = 0; bit < 32; bit++) timestamp = (timestamp * 2 + (pixels[(bit * 20 + 10) * 4] > 128 ? 1 : 0)) >>> 0;
  const phase = context.getImageData(0, 100, 640, 1).data;
  let green = 0;
  for (let x = 0; x < 640; x++) if (phase[x * 4 + 1] > 100 && phase[x * 4] < 100) green++;
  return {ageMs: ((Date.now() >>> 0) - timestamp) >>> 0, motion: green > 500};
})()`;
try {
  await call('Page.navigate', {url: `https://127.0.0.1:${mediaPort}/?${mode}`});
  const started = Date.now();
  const samples = [];
  while (Date.now() - started < 38000) {
    const sample = await evaluate(sampleExpression);
    if (sample) samples.push({elapsedMs: Date.now() - started, ...sample});
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  const stats = await evaluate(`(async () => {
    const inbound = window.__phantom_pc ? Array.from((await window.__phantom_pc.getStats()).values())
      .filter(s => s.type === 'inbound-rtp').map(s => ({kind:s.kind, framesDecoded:s.framesDecoded,
        framesDropped:s.framesDropped, packetsLost:s.packetsLost, totalSamplesReceived:s.totalSamplesReceived,
        concealedSamples:s.concealedSamples, totalAudioEnergy:s.totalAudioEnergy})) : [];
    return {debug:window.__phantom_debug, inbound};
  })()`);
  const moving = samples.filter(s => s.motion);
  const ages = moving.map(s => s.ageMs).sort((a, b) => a - b);
  const idle = samples.filter(s => !s.motion && s.ageMs >= 15000 && s.ageMs < 23000);
  const motionSpanMs = moving.length ? moving.at(-1).elapsedMs - moving[0].elapsedMs : 0;
  const result = {
    mode, samples: samples.length, idleSamplesOver15s: idle.length,
    motionSamples: moving.length, motionSpanMs,
    firstMotionMs: moving[0]?.elapsedMs,
    motionAgeP95Ms: ages[Math.floor(ages.length * 0.95)] ?? null,
    motionAgeMaxMs: ages.at(-1) ?? null,
    stats,
    passed: idle.length > 0 && motionSpanMs >= 6000 && ages.length >= 30 && ages.at(-1) < 1000,
  };
  console.log(JSON.stringify(result, null, 2));
  if (!result.passed) process.exitCode = 1;
} finally {
  socket.close();
  await fetch(`${debugUrl}/json/close/${page.id}`);
}
