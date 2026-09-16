// Calibrate the PCM probe independently of browsers and the product decoder.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';
const source=await fs.readFile(new URL('./media-soak-monitor.js',import.meta.url),'utf8');
const code=source.split(' const code=`')[1].split('`;')[0];
function measure(signal,seconds=1){
 const bins=[];let Tap;
 const env={sampleRate:48000,currentTime:0,AudioWorkletProcessor:class{constructor(){this.port={postMessage:b=>bins.push(b)};}},registerProcessor:(_,cls)=>{Tap=cls}};
 vm.runInNewContext(code,env);const tap=new Tap();
 for(let q=0;q<seconds*375;q++){
  env.currentTime=q*128/48000;
  const a=Float32Array.from({length:128},(_,i)=>signal(q*128+i));
  const out=[new Float32Array(128),new Float32Array(128)];
  tap.process([[a,a]],[out]);assert.deepEqual(out[0],a);assert.deepEqual(out[1],a);
 }
 return bins;
}
const tone=i=>.035*Math.sin(2*Math.PI*440*i/48000);
const pure=measure(tone)[0];assert.equal(pure.frequency,440);assert.equal(pure.silentQuanta,0);
// Brief sign inversions add crossings without changing the dominant tone.
const perturbed=measure(i=>i%1000>=400&&i%1000<410?-tone(i):tone(i))[0];
assert.equal(perturbed.frequency,440);assert.ok(perturbed.zeroCrossingFrequency>460);
const silent=measure(()=>0)[0];assert.equal(silent.rms,0);assert.equal(silent.maxSilentQuanta,375);
const gap=measure(i=>i>=47000&&i<53000?0:tone(i),2);
assert.ok(gap[1].maxSilentQuanta*128/48000*1000>=120);
console.log(JSON.stringify({passed:true,pureHz:pure.frequency,perturbedHz:perturbed.frequency,perturbedCrossings:perturbed.zeroCrossingFrequency,silenceMs:silent.maxSilentQuanta*128/48,crossBinGapMs:gap[1].maxSilentQuanta*128/48}));
