// Inject before Phantom loads. WSS audio is measured by a passthrough worklet;
// RTC audio uses a separate, muted receiver-stream branch (native stats remain primary).
(()=>{
 const state=window.__soak={bins:[],errors:[],contexts:[],connections:0,webSockets:[],mediaEvents:[]};
 navigator.mediaDevices?.addEventListener('devicechange',()=>state.mediaEvents.push({kind:'audio-device',event:'devicechange',wallMs:Date.now()}));
 const NativeSocket=window.WebSocket;
 window.WebSocket=class extends NativeSocket{constructor(...args){super(...args);state.webSockets.push(this);this.__soakMessages=0;this.__soakBytes=0;this.addEventListener('message',e=>{this.__soakMessages++;this.__soakBytes+=e.data?.byteLength??e.data?.size??e.data?.length??0;});}};
 const Base=window.AudioContext, connect=AudioNode.prototype.connect;
 const code=`class SoakTap extends AudioWorkletProcessor {
 constructor(){super();this.n=0;this.sum=0;this.peak=0;this.zeros=0;this.run=0;this.maxRun=0;this.crossings=0;this.prev=0;this.index=0;
  this.bands=Array.from({length:17},(_,i)=>({hz:400+i*5,c:2*Math.cos(2*Math.PI*(400+i*5)/sampleRate),a:0,b:0}));}
 process(inputs,outputs){const src=inputs[0]||[],out=outputs[0]||[],a=src[0];let peak=0;
  for(let c=0;c<out.length;c++){if(src[c])out[c].set(src[c]);else if(a)out[c].set(a);else out[c].fill(0);}
  const n=out[0]?.length||128;
  for(let i=0;i<n;i++){const v=a?.[i]||0;this.sum+=v*v;peak=Math.max(peak,Math.abs(v));if(v>=0&&this.prev<0)this.crossings++;this.prev=v;
   for(const b of this.bands){const next=v+b.c*b.a-b.b;b.b=b.a;b.a=next;}}
  this.n+=n;this.peak=Math.max(this.peak,peak);if(peak<1e-5){this.zeros++;this.run++;this.maxRun=Math.max(this.maxRun,this.run);}else this.run=0;
  if(this.n>=sampleRate){let power=-1,frequency=0;for(const b of this.bands){const p=b.a*b.a+b.b*b.b-b.c*b.a*b.b;if(p>power){power=p;frequency=b.hz;}b.a=0;b.b=0;}
   this.port.postMessage({index:this.index++,audioTime:currentTime,frames:this.n,rms:Math.sqrt(this.sum/this.n),peak:this.peak,silentQuanta:this.zeros,maxSilentQuanta:this.maxRun,frequency,zeroCrossingFrequency:this.crossings*sampleRate/this.n,sampleRate});this.n=0;this.sum=0;this.peak=0;this.zeros=0;this.maxRun=this.run;this.crossings=0;}
  return true;
 }}registerProcessor('soak-tap',SoakTap);`;
 async function setup(ctx,kind){
  ctx.addEventListener('statechange',()=>state.mediaEvents.push({kind,event:'context-state',state:ctx.state,wallMs:Date.now()}));
  state.contexts.push({kind,ctx});const blob=URL.createObjectURL(new Blob([code],{type:'application/javascript'}));
  await ctx.audioWorklet.addModule(blob);URL.revokeObjectURL(blob);
  const tap=new AudioWorkletNode(ctx,'soak-tap',{numberOfInputs:1,numberOfOutputs:1,outputChannelCount:[2]});
  tap.port.onmessage=e=>state.bins.push({...e.data,kind,wallMs:Date.now()});
  connect.call(tap,ctx.destination);return tap;
 }
 window.AudioContext=class extends Base{constructor(...args){super(...args);this.__soakReady=setup(this,'wss').catch(e=>state.errors.push(String(e)));}};
 AudioNode.prototype.connect=function(dest,...args){
  if(dest instanceof AudioDestinationNode&&this.context.__soakReady){
   this.context.__soakReady.then(tap=>{if(tap){connect.call(this,tap,...args);state.connections++;}});return dest;
  }return connect.call(this,dest,...args);
 };
 const observed=new WeakSet(),elements=new WeakSet();
 state.timer=setInterval(async()=>{
  for(const el of document.querySelectorAll('audio,video'))if(!elements.has(el)){
   elements.add(el);for(const event of ['play','playing','pause','ended','waiting','stalled','error','emptied'])el.addEventListener(event,()=>state.mediaEvents.push({kind:el.tagName,event,wallMs:Date.now(),paused:el.paused,time:el.currentTime,error:el.error?.message}));
  }
  const pc=window.__phantom_pc;if(!pc)return;
  for(const r of pc.getReceivers())if(r.track?.kind==='audio'&&!observed.has(r.track)){
   observed.add(r.track);
   for(const event of ['mute','unmute','ended'])r.track.addEventListener(event,()=>state.mediaEvents.push({kind:'rtc-track',event,wallMs:Date.now(),muted:r.track.muted,state:r.track.readyState}));
   try{const ctx=new Base({sampleRate:48000}),tap=await setup(ctx,'rtc-branch');
    const gain=ctx.createGain();gain.gain.value=0;tap.disconnect();connect.call(tap,gain);connect.call(gain,ctx.destination);
    const source=ctx.createMediaStreamSource(new MediaStream([r.track]));connect.call(source,tap);await ctx.resume();state.connections++;
   }catch(e){state.errors.push(String(e));}
  }
 },500);
})();
