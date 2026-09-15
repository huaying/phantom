// Optional WSS diagnostic observer. Inject before Phantom; it records decoded PCM,
// arrival times and SAB underflows without changing prefill or output samples.
// WebCodecs timestamps are assigned by the client and do not prove wire continuity.
(()=>{
 const d=window.__audioDiagnostic={arrivals:[],decoded:[],underflows:[],ctrl:null,configuration:null};
 const NativeDecoder=window.AudioDecoder;
 window.AudioDecoder=class extends NativeDecoder {
  constructor(init){
   super({...init,output(data){
    const samples=new Float32Array(data.numberOfFrames);
    data.copyTo(samples,{planeIndex:0,format:'f32-planar'});
    let peak=0,sum=0;
    for(const x of samples){peak=Math.max(peak,Math.abs(x));sum+=x*x;}
    const e={wallMs:Date.now(),time:performance.now(),frames:data.numberOfFrames,peak,rms:Math.sqrt(sum/samples.length)};
    init.output(data);
    e.buffered=d.ctrl?Atomics.load(d.ctrl,2):null;
    d.decoded.push(e);
   }});
  }
  decode(chunk){d.arrivals.push({wallMs:Date.now(),time:performance.now(),bytes:chunk.byteLength});return super.decode(chunk);}
 };
 const NativeBlob=window.Blob;
 window.Blob=class extends NativeBlob {
  constructor(parts=[],options){
   const modified=parts.map(p=>typeof p==='string'&&p.includes('class PhantomSABProcessor')?
    p.replace('this.started = false;','this.started = false; this.port.postMessage({phantomAudioConfig:true,prefillFrames:this.prefill,connectMs:options.processorOptions.connectMs});').replace('if (buffered < frames) {','if (buffered < frames) { this.port.postMessage({phantomAudioUnderflow:true,buffered,frames,prefill:this.prefill,audioTime:currentTime});'):p);
   super(modified,options);
  }
 };
 const NativeNode=window.AudioWorkletNode;
 window.AudioWorkletNode=class extends NativeNode {
  constructor(ctx,name,options){
   super(ctx,name,options);
   if(name==='phantom-sab-audio'){
    d.ctrl=new Uint32Array(options.processorOptions.ctrlBuffer);
    this.port.addEventListener('message',e=>{if(e.data.phantomAudioUnderflow)d.underflows.push({...e.data,wallMs:Date.now()});if(e.data.phantomAudioConfig)d.configuration=e.data;});
    this.port.start();
   }
  }
 };
})();
