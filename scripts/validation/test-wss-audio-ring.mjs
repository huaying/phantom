// Exercise the real WASM PCM producer with a suspended consumer and marked AudioData.
// Usage: node test-wss-audio-ring.mjs CDP-port fixture-port output-directory
import fs from 'node:fs/promises';
import path from 'node:path';
const [cdp='9272',port='9921',root]=process.argv.slice(2);
if(!/^\d+$/.test(cdp)||!/^\d+$/.test(port)||!root)throw Error('CDP-port fixture-port output-directory required');
await fs.mkdir(root,{recursive:true});
const base=`http://127.0.0.1:${cdp}`;
const page=await fetch(`${base}/json/new?about:blank`,{method:'PUT'}).then(r=>r.json());
const ws=new WebSocket(page.webSocketDebuggerUrl);
await new Promise(r=>ws.onopen=r);
let seq=0;const pending=new Map(),errors=[];
ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.method==='Runtime.exceptionThrown')errors.push(m.params.exceptionDetails);const p=pending.get(m.id);if(p){pending.delete(m.id);clearTimeout(p.timer);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);}};
const call=(method,params={})=>new Promise((resolve,reject)=>{const id=++seq,timer=setTimeout(()=>reject(Error('CDP timeout')),10000);pending.set(id,{resolve,reject,timer});ws.send(JSON.stringify({id,method,params}));});
const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw Error(JSON.stringify(r.exceptionDetails));return r.result.value;};
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
try{
 await call('Page.enable');await call('Runtime.enable');
 await call('Page.addScriptToEvaluateOnNewDocument',{source:`if(location.origin==='https://127.0.0.1:${port}'){
 window.__ringProbe={};
 const Decoder=window.AudioDecoder;
 window.AudioDecoder=class extends Decoder{constructor(init){super(init);__ringProbe.output=init.output;}decode(){}};
 const Node=window.AudioWorkletNode;
 window.AudioWorkletNode=class extends Node{constructor(ctx,name,options){super(ctx,name,options);if(name==='phantom-sab-audio'){Object.assign(__ringProbe,{ctx,ctrl:new Uint32Array(options.processorOptions.ctrlBuffer),audio:new Float32Array(options.processorOptions.audioBuffer),size:options.processorOptions.ringSize});}}};
 }`});
 await call('Page.navigate',{url:`https://127.0.0.1:${port}/?wss`});
 const start=Date.now();
 while(!await evaluate('Boolean(window.__ringProbe?.output&&window.__ringProbe?.ctrl)')){if(Date.now()-start>15000)throw Error('SAB producer unavailable');await sleep(100);}
 const result=await evaluate(`(async()=>{
 const p=__ringProbe;await p.ctx.suspend();p.ctrl.fill(0);p.audio.fill(-1);
 const emit=(frames,value)=>{const data=new AudioData({format:'f32-planar',sampleRate:48000,numberOfFrames:frames,numberOfChannels:2,timestamp:0,data:new Float32Array(frames*2).fill(value)});p.output(data);return data.numberOfFrames===0;};
 const checks=[];const check=(name,passed)=>checks.push({name,passed});
 const same=(a,b)=>a.length===b.length&&a.every((v,i)=>v===b[i]);
 let closed=true;for(let i=1;i<=25;i++)closed=emit(960,i/100)&&closed;
 check('exactly full queue has ordered marked packets',p.ctrl[2]===24000&&Array.from(p.audio).every((v,i)=>v===Math.fround((Math.floor(i/1920)+1)/100)));
 const before=Array.from(p.audio),ctrl=Array.from(p.ctrl);
 closed=emit(960,.99)&&closed;
 check('full queue preserves every unplayed sample',same(Array.from(p.audio),before)&&same(Array.from(p.ctrl),ctrl));
 Atomics.store(p.ctrl,1,128);Atomics.sub(p.ctrl,2,128);
 const partial=Array.from(p.ctrl);closed=emit(960,.98)&&closed;
 check('partial free space does not accept a partial packet',same(Array.from(p.audio),before)&&same(Array.from(p.ctrl),partial));
 Atomics.store(p.ctrl,1,960);Atomics.sub(p.ctrl,2,832);closed=emit(960,.77)&&closed;
 check('producer resumes into consumed space across ring wrap',p.ctrl[0]===960&&p.ctrl[1]===960&&p.ctrl[2]===24000&&Array.from(p.audio).every((v,i)=>v===(i<1920?Math.fround(.77):before[i])));
 const wrapped=Array.from(p.audio),wrappedCtrl=Array.from(p.ctrl);closed=emit(p.size+1,.97)&&closed;
 check('oversized packet leaves ring and counters untouched',same(Array.from(p.audio),wrapped)&&same(Array.from(p.ctrl),wrappedCtrl));
 check('every accepted and dropped AudioData is closed',closed);
 return {passed:checks.every(c=>c.passed),ringFrames:p.size,checks};
 })()`);
 result.errors=errors;result.passed&&=errors.length===0;
 await fs.writeFile(path.join(root,'wss-audio-ring.json'),JSON.stringify(result,null,2));
 console.log(JSON.stringify(result));if(!result.passed)process.exitCode=1;
}finally{ws.close();await fetch(`${base}/json/close/${page.id}`);}
