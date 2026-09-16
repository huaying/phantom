// Replay real decoded-packet timings through the actual embedded SAB worklet.
// Nonzero marker samples test buffering; the live PCM soak tests tone quality.
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
const root=new URL('../../',import.meta.url);
const rust=fs.readFileSync(new URL('crates/web/src/lib.rs',root),'utf8');
const code=rust.match(/let worklet_code = r#"\s*(class PhantomSABProcessor[\s\S]*?)"#;/)?.[1];
assert(code,'embedded SAB worklet must be present');
const fixture=JSON.parse(fs.readFileSync(new URL('fixtures/wss-audio-arrivals.json',import.meta.url),'utf8'));
function replay(events,connectMs){
 let Processor;
 const context={AudioWorkletProcessor:class{constructor(){this.port={postMessage(){}};}},registerProcessor(_name,C){Processor=C;},currentTime:0};
 vm.runInNewContext(code,context,{filename:fileURLToPath(new URL('crates/web/src/lib.rs',root))});
 const ctrlBuffer=new SharedArrayBuffer(12),audioBuffer=new SharedArrayBuffer(24000*2*4),ctrl=new Uint32Array(ctrlBuffer);
 new Float32Array(audioBuffer).fill(.25);
 const p=new Processor({processorOptions:{ctrlBuffer,audioBuffer,ringSize:24000,channels:2,connectMs}});
 const output=[new Float32Array(128),new Float32Array(128)];
 let next=0,run=0,maxRun=0,overflows=0;
 for(let time=0;time<events.at(-1)[0];time+=128/48){
  while(next<events.length&&events[next][0]<=time){
   const frames=events[next++][1];
   if(frames>24000||ctrl[2]>24000-frames){overflows++;continue;}
   ctrl[0]=(ctrl[0]+frames)%24000;ctrl[2]+=frames;
  }
  context.currentTime=time/1000;p.process([], [output],{});
  if(time<5000)continue;
  if(output[0].every(x=>x===0)){run++;maxRun=Math.max(maxRun,run);}else run=0;
 }
 return {prefillMs:p.prefill/48,maxSilentMs:maxRun*128/48,overflows};
}
const oldFloor=replay(fixture.events,0);
const wan=replay(fixture.events,fixture.connectMs);
assert(oldFloor.maxSilentMs>=100,'recorded trace must reproduce the 60ms-buffer regression');
assert(wan.maxSilentMs<100,'measured WAN hint must keep this trace below the unchanged gate');
assert.equal(wan.prefillMs,300);assert.equal(wan.overflows,0);
const steady=Array.from({length:400},(_,i)=>[i*20,960]);
const lan=replay(steady,5);
assert.equal(lan.prefillMs,60);assert.equal(lan.maxSilentMs,0);
for(const badHint of [undefined,NaN,Infinity,-100])assert.equal(replay(steady,badHint).prefillMs,60);
const outage=Array.from({length:500},(_,i)=>[i*20+(i>=300?1000:0),960]);
const bounded=replay(outage,10000);
assert.equal(bounded.prefillMs,300);
assert(bounded.maxSilentMs>=500,'a real long outage must remain observable, not concealed by fabricated samples');
console.log(JSON.stringify({passed:true,oldFloor,wan,lan,longOutage:bounded}));
