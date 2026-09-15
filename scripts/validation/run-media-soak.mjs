// Node 22+. Run against one explicitly selected canary and isolated Chrome.
// Usage: node run-media-soak.mjs rtc|wss host:port seconds cdp-port output-dir
// Optional PHANTOM_SOAK_AUDIO_DIAGNOSTICS=1 records WSS buffer/PCM diagnostics.
import fs from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
const [mode,host,durationArg='1800',port='9271',output] = process.argv.slice(2);
const duration=Number(durationArg);
const audioDiagnostics=process.env.PHANTOM_SOAK_AUDIO_DIAGNOSTICS==='1';
if(audioDiagnostics&&mode!=='wss')throw Error('SAB audio diagnostics require WSS mode');
if(!['rtc','wss'].includes(mode)||!/^([\w.-]+):\d+$/.test(host||'')||!Number.isFinite(duration)||duration<20||!output)throw Error('Usage: rtc|wss host:port seconds cdp-port output-dir');
await fs.mkdir(output,{recursive:true});
const prefix=path.join(output,mode),base=`http://127.0.0.1:${port}`;
const page=await fetch(`${base}/json/new?about:blank`,{method:'PUT'}).then(r=>r.json());
await fs.writeFile(`${prefix}-page.json`,JSON.stringify(page));
const ws=new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject;});
let seq=0;const pending=new Map(),errors=[],ignoredErrors=[],consoleMessages=[];
ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.method==='Runtime.exceptionThrown')errors.push(m.params.exceptionDetails);if(m.method==='Log.entryAdded'&&m.params.entry.level==='error'){
 const entry=m.params.entry;
 // The server does not ship a favicon. Keep this known 404 in the evidence,
 // but do not turn successful media into a failed acceptance run for it.
 const favicon404=entry.source==='network'&&entry.url===`https://${host}/favicon.ico`&&entry.text==='Failed to load resource: the server responded with a status of 404 (Not Found)';
 (favicon404?ignoredErrors:errors).push(entry);
}if(m.method==='Runtime.consoleAPICalled'){const entry={type:m.params.type,timestamp:m.params.timestamp,args:m.params.args.map(x=>x.value??x.description)};consoleMessages.push(entry);if(entry.type==='error')errors.push(entry);}const p=pending.get(m.id);if(p){pending.delete(m.id);clearTimeout(p.timer);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);}};
const call=(method,params={})=>new Promise((resolve,reject)=>{const id=++seq,timer=setTimeout(()=>{pending.delete(id);reject(Error(`${method} timeout`));},15000);pending.set(id,{resolve,reject,timer});ws.send(JSON.stringify({id,method,params}));});
const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw Error(JSON.stringify(r.exceptionDetails));return r.result.value;};
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
const expression=`(async()=>{
 const s=window.__soak;
 const src=document.querySelector('video')||document.querySelector('#screen');
 const w=src?.videoWidth||src?.width,h=src?.videoHeight||src?.height;
 let stamp=null,valid=false,bits='';
 if(w===1920&&h===1080){
  const c=window.__soakPixels||(window.__soakPixels=document.createElement('canvas'));c.width=w;c.height=h;
  const x=c.getContext('2d',{willReadFrequently:true});x.drawImage(src,0,0,w,h);const p=x.getImageData(0,96,w,1).data;
  for(let i=0;i<64;i++){const n=(96+24*i+12)*4;bits+=(p[n]+p[n+1]+p[n+2])/3>128?'1':'0';}
  valid=bits.startsWith('10100101')&&bits.endsWith('01011010');if(valid)stamp=parseInt(bits.slice(8,56),2);
 }
 const stats=window.__phantom_pc?Array.from((await window.__phantom_pc.getStats()).values()).filter(x=>x.type==='inbound-rtp').map(x=>Object.fromEntries(['kind','id','timestamp','framesDecoded','framesDropped','packetsLost','packetsReceived','bytesReceived','totalSamplesReceived','concealedSamples','silentConcealedSamples','concealmentEvents','totalAudioEnergy','jitter','jitterBufferDelay','jitterBufferEmittedCount','freezeCount','totalFreezesDuration'].filter(k=>x[k]!==undefined).map(k=>[k,x[k]]))):[];
 return {audioDiagnostic:window.__audioDiagnostic?{configuration:window.__audioDiagnostic.configuration,arrivals:window.__audioDiagnostic.arrivals.splice(0),decoded:window.__audioDiagnostic.decoded.splice(0),underflows:window.__audioDiagnostic.underflows.splice(0),buffered:window.__audioDiagnostic.ctrl?Atomics.load(window.__audioDiagnostic.ctrl,2):null}:undefined,wallMs:Date.now(),visibility:document.visibilityState,width:w,height:h,valid,stamp,ageMs:stamp===null?null:Date.now()-stamp,video:src?.getVideoPlaybackQuality?(()=>{const v=src.getVideoPlaybackQuality();return {totalVideoFrames:v.totalVideoFrames,droppedVideoFrames:v.droppedVideoFrames,corruptedVideoFrames:v.corruptedVideoFrames};})():null,pcState:window.__phantom_pc?.connectionState,stats,debug:window.__phantom_debug,audioBins:s?.bins.splice(0)||[],monitorErrors:s?.errors||[],contexts:s?.contexts.map(({kind,ctx})=>({kind,state:ctx.state,sampleRate:ctx.sampleRate,currentTime:ctx.currentTime}))||[],webSockets:s?.webSockets.map(w=>({path:new URL(w.url).pathname,state:w.readyState,messages:w.__soakMessages,bytes:w.__soakBytes}))||[],connections:s?.connections||0,mediaEvents:s?.mediaEvents||[],mediaElements:Array.from(document.querySelectorAll('audio,video')).map(a=>({kind:a.tagName,paused:a.paused,ended:a.ended,ready:a.readyState,time:a.currentTime,muted:a.muted,error:a.error?.message})),rtcTracks:window.__phantom_pc?.getReceivers().map(r=>({kind:r.track.kind,muted:r.track.muted,state:r.track.readyState}))||[]};
})()`;
const quantile=(arr,q)=>arr.length?[...arr].sort((a,b)=>a-b)[Math.min(arr.length-1,Math.floor(arr.length*q))]:null;
const samples=[];let result,abortReason=null;
const keepFailed=process.env.PHANTOM_SOAK_KEEP_FAILED==='1';
try{
 await call('Page.enable');await call('Runtime.enable');await call('Log.enable');
 const validationDir=path.dirname(fileURLToPath(import.meta.url));
 const monitor=(audioDiagnostics?await fs.readFile(path.join(validationDir,'wss-audio-diagnostic.js'),'utf8')+'\n':'')+await fs.readFile(path.join(validationDir,'media-soak-monitor.js'),'utf8');
 await call('Page.addScriptToEvaluateOnNewDocument',{source:`if(location.origin==='https://${host}'){`+monitor+'}'});
 await call('Page.navigate',{url:`https://${host}/?${mode}`});
 await call('Page.bringToFront');await sleep(5000);
 // Establish the normal user gesture used to start remote audio playback.
 for(const type of ['mousePressed','mouseReleased'])await call('Input.dispatchMouseEvent',{type,x:720,y:400,button:'left',clickCount:1});
 await sleep(5000);
 const initial=await evaluate(expression);
 const pic=await call('Page.captureScreenshot',{format:'png'});await fs.writeFile(`${prefix}-start.png`,Buffer.from(pic.data,'base64'));
 await fs.writeFile(`${prefix}-initial.json`,JSON.stringify(initial,null,2));
 if(!initial.valid||initial.connections<1||initial.contexts.some(x=>x.state!=='running'))throw Error(`Preflight failed: valid=${initial.valid} connections=${initial.connections} contexts=${JSON.stringify(initial.contexts)}`);
 const begin=Date.now();let next=begin,badSamples=0;
 while(Date.now()-begin<duration*1000){
  const x=await evaluate(expression);x.elapsedMs=Date.now()-begin;samples.push(x);await fs.appendFile(`${prefix}-samples.jsonl`,JSON.stringify(x)+'\n');
  if(samples.length%30===0)console.log(JSON.stringify({mode,elapsedSec:Math.round(x.elapsedMs/1000),ageMs:x.ageMs,audioBins:x.audioBins.length,valid:x.valid}));
  if(x.connections!==initial.connections){abortReason='Unrequested media reconnection during the soak';break;}
  const stalled=!x.valid||x.ageMs>5000||(mode==='rtc'&&x.pcState!=='connected')||x.audioBins.some(b=>b.maxSilentQuanta*128/b.sampleRate>5);
  badSamples=stalled?badSamples+1:0;
  if(badSamples>=5){abortReason='Sustained media failure in five consecutive samples';break;}
  next+=1000;await sleep(Math.max(0,next-Date.now()));
 }
 const final=await evaluate(expression);final.elapsedMs=Date.now()-begin;samples.push(final);await fs.appendFile(`${prefix}-samples.jsonl`,JSON.stringify(final)+'\n');
 const ages=samples.filter(x=>x.valid).map(x=>x.ageMs),bins=samples.flatMap(x=>x.audioBins);
 const valid=samples.filter(x=>x.valid).length,active=bins.filter(x=>x.rms>.001&&x.frequency>420&&x.frequency<460);
 const firstAudio=initial.stats.find(x=>x.kind==='audio'),lastAudio=final.stats.find(x=>x.kind==='audio');
 const received=lastAudio&&firstAudio?lastAudio.totalSamplesReceived-firstAudio.totalSamplesReceived:null;
 const concealed=lastAudio&&firstAudio?lastAudio.concealedSamples-firstAudio.concealedSamples:null;
 result={mode,host,audioDiagnostics,requestedDurationSec:duration,abortReason,startedUtc:new Date(begin).toISOString(),durationSec:(Date.now()-begin)/1000,samples:samples.length,validSamples:valid,frameAgeMs:{min:quantile(ages,0),p50:quantile(ages,.5),p95:quantile(ages,.95),p99:quantile(ages,.99),max:quantile(ages,1)},audio:{bins:bins.length,activeBins:active.length,silentQuanta:bins.reduce((n,b)=>n+b.silentQuanta,0),maxSilentMs:Math.max(0,...bins.map(b=>b.maxSilentQuanta*128/b.sampleRate*1000)),received,concealed,concealmentRatio:received?concealed/received:null},initial,final,errors,ignoredErrors,consoleMessages};
 result.passed=abortReason===null&&valid===samples.length&&result.durationSec>=duration&&ages.every(x=>x>=-1000&&x<2000)&&bins.length>=duration-5&&active.length===bins.length&&result.audio.maxSilentMs<100&&errors.length===0&&final.monitorErrors.length===0&&result.frameAgeMs.p95<1000&&(mode!=='rtc'||(received>0&&concealed>=0&&result.audio.concealmentRatio<.01&&samples.every(x=>x.pcState==='connected'&&x.stats.find(y=>y.kind==='audio')?.id===firstAudio.id)));
 await fs.writeFile(`${prefix}-result.json`,JSON.stringify(result,null,2));
 const lastPic=await call('Page.captureScreenshot',{format:'png'});await fs.writeFile(`${prefix}-end.png`,Buffer.from(lastPic.data,'base64'));
 console.log(JSON.stringify({mode,passed:result.passed,durationSec:result.durationSec,frameAgeMs:result.frameAgeMs,audio:result.audio,errors:errors.length}));
 if(!result.passed)process.exitCode=1;
}catch(e){await fs.writeFile(`${prefix}-error.txt`,String(e));console.error(String(e));process.exitCode=1;}
finally{await fs.writeFile(`${prefix}-console.json`,JSON.stringify(consoleMessages,null,2));ws.close();if(!keepFailed||result?.passed)await fetch(`${base}/json/close/${page.id}`).catch(()=>{});else console.log(JSON.stringify({keptFailedPage:page.id}));}
