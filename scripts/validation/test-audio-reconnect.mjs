// Local transport_smoke fixture only. Node 22+.
// Usage: node test-audio-reconnect.mjs CDP-port fixture-port output-directory
import fs from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
const [cdpPort='9272',fixturePort='9921',root]=process.argv.slice(2);
if(!/^\d+$/.test(cdpPort)||!/^\d+$/.test(fixturePort)||!root)throw Error('Usage: CDP-port fixture-port output-directory');
await fs.mkdir(root,{recursive:true});
const base=`http://127.0.0.1:${cdpPort}`;
const monitor=await fs.readFile(path.join(path.dirname(fileURLToPath(import.meta.url)),'media-soak-monitor.js'),'utf8');
const results=[];
for(const scenario of ['delayed-sab','buffer-source']){
 const page=await fetch(`${base}/json/new?about:blank`,{method:'PUT'}).then(r=>r.json());
 const ws=new WebSocket(page.webSocketDebuggerUrl);await new Promise(r=>ws.onopen=r);let seq=0;const pending=new Map(),errors=[];
 ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.method==='Runtime.exceptionThrown')errors.push(m.params.exceptionDetails);if(m.method==='Runtime.consoleAPICalled'&&m.params.type==='error')errors.push(m.params.args.map(a=>a.value));const p=pending.get(m.id);if(p){pending.delete(m.id);clearTimeout(p.t);m.error?p.j(Error(JSON.stringify(m.error))):p.r(m.result);}};
 const call=(method,params={})=>new Promise((r,j)=>{const id=++seq,t=setTimeout(()=>j(Error('CDP timeout')),10000);pending.set(id,{r,j,t});ws.send(JSON.stringify({id,method,params}));});
 const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw Error(JSON.stringify(r.exceptionDetails));return r.result.value;};
 const sleep=ms=>new Promise(r=>setTimeout(r,ms));
 const until=async expression=>{const start=Date.now();while(Date.now()-start<15000){if(await evaluate(expression))return;await sleep(100);}throw Error('condition timeout: '+expression);};
 const snapshot=()=>evaluate(`({contexts:__soak.contexts.map(({kind,ctx})=>({kind,state:ctx.state})),bins:__soak.bins.filter(b=>b.wallMs>Date.now()-2500),debug:__phantom_debug,earlyFocus:window.__earlyFocus||[],delayed:window.__delayedModules||0,sockets:__soak.webSockets.map(w=>({path:new URL(w.url).pathname,state:w.readyState})),monitorErrors:__soak.errors})`);
 const closeMain=()=>evaluate(`(()=>{const w=__soak.webSockets.find(w=>w.readyState===1&&new URL(w.url).pathname==='/ws');if(!w)throw Error('no main socket');w.close(1000,'validation reconnect');return true})()`);
 const samples=[];
 try{
  await call('Page.enable');await call('Runtime.enable');
  const prelude=scenario==='buffer-source'?`Object.defineProperty(window,'SharedArrayBuffer',{value:undefined});`:`(()=>{const original=AudioWorklet.prototype.addModule;window.__delayedModules=0;AudioWorklet.prototype.addModule=function(url,opts){const result=original.call(this,url,opts);return fetch(url).then(r=>r.text()).then(code=>{if(code.includes('PhantomSABProcessor')&&__delayedModules===0){__delayedModules++;return result.then(()=>new Promise(r=>setTimeout(r,5000)));}return result;});};})();`;
  const earlyFocus=`(()=>{const WS=window.WebSocket;window.__earlyFocus=[];window.WebSocket=class extends WS {constructor(...args){super(...args);if(new URL(args[0]).pathname==='/ws')setTimeout(()=>{__earlyFocus.push(this.readyState);window.dispatchEvent(new Event('focus'));},0);}};})();`;
  await call('Page.addScriptToEvaluateOnNewDocument',{source:`if(location.origin==='https://127.0.0.1:${fixturePort}'){`+earlyFocus+prelude+monitor+'}'});
  await call('Page.navigate',{url:`https://127.0.0.1:${fixturePort}/?wss`});
  await until(`window.__soak?.contexts.length===1&&__soak.webSockets.some(w=>w.readyState===1&&new URL(w.url).pathname==='/ws')${scenario==='delayed-sab'?'&&window.__delayedModules===1':''}`);
  await until(`__phantom_debug.cursorAppliedShapeId>0`);
  let previousCursor=(await snapshot()).debug.cursorAppliedShapeId;
  for(let i=0;i<3;i++){
   await closeMain();await until(`__soak.contexts.length===${i+2}&&__soak.contexts.at(-1).ctx.state==='running'`);await sleep(6500);
   const s=await snapshot();samples.push(s);
   const freshCursor=s.debug.cursorShapeId>0&&s.debug.cursorShapeId!==previousCursor&&s.debug.cursorAppliedShapeId===s.debug.cursorShapeId;
   previousCursor=s.debug.cursorAppliedShapeId;
   const active=s.contexts.filter(c=>c.state==='running').length;
   const activeTone=s.bins.length>=2&&s.bins.every(b=>b.rms>.001&&b.frequency>420&&b.frequency<460);
   console.log(JSON.stringify({scenario,cycle:i+1,contexts:s.contexts,activeTone,freshCursor,gotKeyframe:s.debug.gotKeyframe,earlyFocus:s.earlyFocus,errors:errors.length}));
   if(!freshCursor||!s.debug.gotKeyframe||!s.earlyFocus.includes(0)||active!==1||s.contexts.slice(0,-1).some(c=>c.state!=='closed')||!activeTone||s.monitorErrors.length||errors.length)throw Error('WSS audio/cursor lifecycle regression');
  }
  results.push({scenario,passed:true,samples,errors});
 }catch(e){results.push({scenario,passed:false,error:String(e),samples,errors});process.exitCode=1;}
 finally{ws.close();await fetch(`${base}/json/close/${page.id}`);}
}
await fs.writeFile(`${root}/local-wss-audio-lifecycle.json`,JSON.stringify(results,null,2));
