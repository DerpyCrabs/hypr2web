'use strict';
const $ = id => document.getElementById(id);
const canvas = $('screen'), video = $('video'), ctx = canvas.getContext('2d', {alpha: false});
let ws, pc, decoder, generation = 0, received = new Map(), drawn = 0, lastDrawn = 0;
let lastTick = performance.now(), currentTransport = 'webrtc', previousStats = null, vfCallback = 0;
let viewportTimer, lastViewport='';
let h264Profile='main';
let connectionTimer, feedbackBusy = false, localIce = [], remoteIce = [], answerSent = false;
const h2w = window.h2w = {
    frames: 0, captureMs: 0, encodeMs: 0, drawMs: 0, decodeMs: 0,
    rtt: 0, ackMs: 0, jitterMs: 0, loss: 0, bitrate: 0, errors: [], fps: 0,
};
const query = new URLSearchParams(location.search);
for (const id of ['rate','bitrate','transport','decodeMode']) {
    const saved = query.get(id) || localStorage.getItem('hypr2web.' + id);
    if (saved && [...$(id).options].some(o => o.value === saved)) $(id).value = saved;
}
$('adaptive').checked=query.get('adaptive')==='1';
if (query.get('transport') === 'ws') $('transport').value = 'ws';
const resetTouchInputs = [];
function send(value) {
    if (value.type === 'release') resetTouchInputs.forEach(reset => reset());
    if (ws?.readyState === 1) ws.send(JSON.stringify(value));
}
function fail(error) {
    const text = error.message || String(error);
    h2w.errors.push(text); $('error').textContent = text;
    disconnect(); document.body.classList.add('has-error');
}
function disconnect() {
    generation++;
    send({type:'release'});
    clearTimeout(connectionTimer);clearTimeout(viewportTimer);lastViewport='';
    if (ws) { ws.onclose = null; ws.close(); ws = null; }
    if (pc) { pc.close(); pc = null; }
    if (decoder && decoder.state !== 'closed') decoder.close();
    decoder = null;
    if (vfCallback && video.cancelVideoFrameCallback) video.cancelVideoFrameCallback(vfCallback);
    vfCallback = 0; video.srcObject = null; received.clear(); previousStats = null;
    document.body.classList.remove('connected');
    $('status').textContent = 'Disconnected';
}
function fitCanvas() {
    const r = $('stage').getBoundingClientRect();
    const el = currentTransport === 'webrtc' ? video : canvas;
    const w = currentTransport === 'webrtc' ? video.videoWidth || 1920 : canvas.width;
    const h = currentTransport === 'webrtc' ? video.videoHeight || 1080 : canvas.height;
    const scale = Math.min(r.width / w, r.height / h);
    el.style.width = w * scale + 'px'; el.style.height = h * scale + 'px';
}
function viewportSize() {
    const r=$('stage').getBoundingClientRect();
    const dpr=window.devicePixelRatio || 1;
    const bound=n=>Math.max(128,Math.min(8192,Math.floor(n*dpr/2)*2));
    return {width:bound(r.width),height:bound(r.height)};
}
function updateViewport() {
    if(ws?.readyState!==1)return;
    const size=viewportSize(), signature=JSON.stringify(size);
    if(signature!==lastViewport){send({type:'viewport',...size});lastViewport=signature;}
}
function scheduleViewport() {
    clearTimeout(viewportTimer);viewportTimer=setTimeout(updateViewport,300);
}
new ResizeObserver(()=>{fitCanvas();scheduleViewport();}).observe($('stage'));
// DPR may change when moving between displays without a CSS resize.
function watchPixelRatio() {
    matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener('change',()=>{
        scheduleViewport();watchPixelRatio();
    },{once:true});
}
watchPixelRatio();
video.addEventListener('resize', fitCanvas);
video.addEventListener('loadeddata',()=>{if(currentTransport==='webrtc'&&video.srcObject) connected();});
function connected() {
    clearTimeout(connectionTimer);
    $('status').textContent = 'Connected';
    document.body.classList.add('connected');
}
function countVideoFrame(g) {
    if (!video.requestVideoFrameCallback) return;
    vfCallback = video.requestVideoFrameCallback((now, metadata) => {
        if (g !== generation) return;
        drawn++; h2w.frames++; h2w.videoWidth = metadata.width; h2w.videoHeight = metadata.height;
        if (Number.isFinite(metadata.processingDuration)) h2w.processingMs = metadata.processingDuration * 1000;
        connected(); countVideoFrame(g);
    });
}
function nals(data) {
    const out = [];
    for (let i=0; i+4<data.length; i++) {
        let n=0;
        if (data[i]===0 && data[i+1]===0) {
            if (data[i+2]===1) n=3;
            else if (data[i+2]===0 && data[i+3]===1) n=4;
        }
        if(n) {out.push(i+n); i+=n-1;}
    }
    return out;
}
async function decodePacket(buffer, g) {
    const h = new DataView(buffer);
    if (h.byteLength < 25 || h.getUint32(0,true) !== 0x31425748) throw Error('Invalid video packet');
    const seq=h.getUint32(4,true), data=new Uint8Array(buffer,24), units=nals(data);
    const key=units.some(i=>(data[i]&31)===5);
    if (!decoder) {
        const sps=units.find(i=>(data[i]&31)===7);
        if (sps===undefined || !key) throw Error('Missing initial keyframe');
        const codec='avc1.'+[data[sps+1],data[sps+2],data[sps+3]].map(v=>v.toString(16).padStart(2,'0')).join('');
        let config={codec,optimizeForLatency:true,hardwareAcceleration:$('decodeMode').value};
        if (config.hardwareAcceleration==='auto') {
            config.hardwareAcceleration='prefer-hardware';
            const support=await VideoDecoder.isConfigSupported(config);
            if (!support.supported) config.hardwareAcceleration='no-preference';
        }
        const support=await VideoDecoder.isConfigSupported(config);
        if (g!==generation) return;
        if (!support.supported) throw Error('H.264 decoder configuration unsupported; try WebRTC or lower resolution');
        h2w.decoder=config.hardwareAcceleration;
        decoder=new VideoDecoder({
            output: frame => {
                if (g!==generation) {frame.close();return;}
                const seq=frame.timestamp, m=received.get(seq);
                const now=performance.now();
                if(canvas.width!==frame.displayWidth || canvas.height!==frame.displayHeight) {
                    canvas.width=frame.displayWidth;canvas.height=frame.displayHeight;fitCanvas();
                }
                // Draw immediately: no extra animation-frame wait before acknowledging.
                ctx.drawImage(frame,0,0);frame.close();
                if(m) {
                    h2w.captureMs=m.capture;h2w.encodeMs=m.encode;
                    h2w.decodeMs=now-m.at;h2w.drawMs=performance.now()-now;
                }
                for(const k of received.keys()) if(k<=seq) received.delete(k);
                send({type:'ack',seq});drawn++;h2w.frames++;h2w.lastSeq=seq;connected();
            },
            error: e => {if(g===generation) fail(e);}
        });
        decoder.configure(config);h2w.codec=codec;
    }
    if(g!==generation) return;
    received.set(seq,{capture:h.getUint32(8,true)/1000,encode:h.getUint32(12,true)/1000,at:performance.now()});
    decoder.decode(new EncodedVideoChunk({type:key?'key':'delta',timestamp:seq,data}));
}
async function handleMessage(e,g) {
    if(g!==generation) return;
    if(typeof e.data!=='string') {await decodePacket(e.data,g);return;}
    const v=JSON.parse(e.data);
    if(v.type==='error') {fail(v.message);return;}
    if(v.type==='stream') {
        h2w.stream=v;
        if(navigator.mediaCapabilities && currentTransport==='webrtc') {
            navigator.mediaCapabilities.decodingInfo({type:'webrtc',video:{
                contentType:'video/H264;profile-level-id='+(h264Profile==='main'?'4d0034':'42e034')+';packetization-mode=1',
                width:v.width,height:v.height,bitrate:v.bitrate,framerate:v.fps,
            }}).then(info=>{if(g===generation) h2w.capability={supported:info.supported,smooth:info.smooth,powerEfficient:info.powerEfficient};}).catch(()=>{});
        }
        return;
    }
    if(v.type==='stats') {
        h2w.ackMs=v.ackMs;h2w.bitrate=v.bitrate;
        if(currentTransport==='webrtc') {h2w.captureMs=v.captureMs;h2w.encodeMs=v.encodeMs;}
        return;
    }
    if(v.type==='pong') {h2w.rtt=performance.now()-v.at;return;}
    if(v.type==='ice' && pc) {
        if(pc.remoteDescription) await pc.addIceCandidate(v.candidate);
        else remoteIce.push(v.candidate);
        return;
    }
    if(v.type==='offer' && pc) {
        const peer=pc;
        await peer.setRemoteDescription(v.sdp);
        for(const candidate of remoteIce) await peer.addIceCandidate(candidate);
        remoteIce=[];
        for(const receiver of peer.getReceivers()) {
            if('jitterBufferTarget' in receiver) receiver.jitterBufferTarget=0;
            if('playoutDelayHint' in receiver) receiver.playoutDelayHint=0;
        }
        await peer.setLocalDescription(await peer.createAnswer());
        if(g!==generation) return;
        send({type:'answer',sdp:peer.localDescription});answerSent=true;
        for(const candidate of localIce) send({type:'ice',candidate});
        localIce=[];
    }
}
async function connect() {
    disconnect();$('error').textContent='';document.body.classList.remove('has-error');
    const g=generation;
    currentTransport=$('transport').value;h2w.transport=currentTransport;
    h2w.decodeMs=0;h2w.jitterMs=0;h2w.loss=0;h2w.decoder='pending';h2w.receivedFps=0;h2w.capability=null;
    $('decodeMode').closest('label').hidden=currentTransport==='webrtc';
    canvas.hidden=currentTransport==='webrtc';video.hidden=currentTransport!=='webrtc';
    fitCanvas();
    if(currentTransport==='ws' && !window.VideoDecoder) {fail('WebCodecs requires a supported browser and HTTPS');return;}
    await new Promise(r=>setTimeout(r,350));if(g!==generation) return;
    localIce=[];remoteIce=[];answerSent=false;
    if(currentTransport==='webrtc') {
        const codecs=RTCRtpReceiver.getCapabilities('video')?.codecs||[];
        h264Profile=codecs.some(c=>c.mimeType.toLowerCase()==='video/h264'&&/profile-level-id=4d/i.test(c.sdpFmtpLine||''))?'main':'baseline';
        pc=new RTCPeerConnection({iceServers:[]});
        const peer=pc;
        peer.onicecandidate=e=>{
            if(g!==generation || !e.candidate) return;
            if(answerSent) send({type:'ice',candidate:e.candidate});
            else localIce.push(e.candidate);
        };
        peer.ontrack=e=>{
            if(g!==generation) return;
            video.srcObject=e.streams[0] || new MediaStream([e.track]);
            video.play().catch(e=>{if(g===generation) fail(e);});
            countVideoFrame(g);
        };
        peer.onconnectionstatechange=()=>{
            if(g===generation && peer.connectionState==='failed')
                fail('WebRTC connection failed. Check LAN UDP access or select WebSocket in settings.');
        };
    }
    const url=new URL('/ws',location.href);
    url.protocol=location.protocol==='https:'?'wss:':'ws:';
    url.search=new URLSearchParams({
        ...viewportSize(),fps:$('rate').value,bitrate:$('bitrate').value,
        transport:currentTransport,adaptive:$('adaptive').checked,
        gpu:query.get('gpu')!=='0',profile:currentTransport==='webrtc'?h264Profile:'main',
    });
    lastViewport=JSON.stringify(viewportSize());
    ws=new WebSocket(url);ws.onopen=()=>{if(g===generation)updateViewport();};ws.binaryType='arraybuffer';$('status').textContent='Connecting';
    ws.onclose=()=>{if(g===generation) disconnect();};
    ws.onerror=()=>{if(g===generation) fail('Connection failed');};
    let chain=Promise.resolve();
    ws.onmessage=e=>{chain=chain.then(()=>handleMessage(e,g)).catch(e=>{if(g===generation) fail(e);});};
    connectionTimer=setTimeout(()=>{if(g===generation) fail('No video received. Try WebSocket in settings.');},12000);
}
async function collectStats() {
    if(!pc || pc.connectionState!=='connected' || feedbackBusy) return;
    feedbackBusy=true;
    const peer=pc,g=generation;
    try {
        const report=await peer.getStats();if(g!==generation) return;
        let inbound;
        report.forEach(s=>{
            if(s.type==='inbound-rtp' && s.kind==='video') inbound=s;
            if(s.type==='candidate-pair' && s.state==='succeeded' && s.nominated)
                h2w.mediaRttMs=(s.currentRoundTripTime||0)*1000;
        });
        if(!inbound) return;
        const s=inbound, p=previousStats;
        if(s.framesDecoded>0) connected();
        h2w.decoder=s.decoderImplementation || 'browser video';
        h2w.powerEfficient=s.powerEfficientDecoder;
        if(p) {
            const frames=s.framesDecoded-p.framesDecoded, emitted=s.jitterBufferEmittedCount-p.jitterBufferEmittedCount;
            h2w.decodeMs=frames>0?(s.totalDecodeTime-p.totalDecodeTime)*1000/frames:0;
            h2w.jitterMs=emitted>0?(s.jitterBufferDelay-p.jitterBufferDelay)*1000/emitted:0;
            const lost=Math.max(0,s.packetsLost-p.packetsLost), received=Math.max(0,s.packetsReceived-p.packetsReceived);
            h2w.loss=lost/Math.max(1,lost+received);
            h2w.actualMbps=(s.bytesReceived-p.bytesReceived)*8/(s.timestamp-p.timestamp)/1000;
            h2w.receivedFps=frames*1000/(s.timestamp-p.timestamp);h2w.decodedFrames=s.framesDecoded;
            send({type:'feedback',decode_ms:h2w.decodeMs,jitter_ms:h2w.jitterMs,loss:h2w.loss,rtt_ms:h2w.mediaRttMs||h2w.rtt});
        }
        previousStats=s;
    } catch(e) {console.debug('Stats unavailable',e);} finally {feedbackBusy=false;}
}
$('connect').onclick=connect;$('pause').onclick=disconnect;
$('release').onclick=()=>send({type:'release'});
$('fullscreen').onclick=()=>{if(document.fullscreenElement) document.exitFullscreen();else document.documentElement.requestFullscreen().catch(fail);};
$('layout').onchange=()=>send({type:'layout',group:Number($('layout').value)});
for(const id of ['rate','bitrate','transport','decodeMode']) $(id).onchange=()=>{
    localStorage.setItem('hypr2web.'+id,$(id).value);connect();
};
$('adaptive').onchange=connect;
function point(e) {const r=e.currentTarget.getBoundingClientRect();send({type:'pointer',x:(e.clientX-r.left)/r.width,y:(e.clientY-r.top)/r.height});}
for(const target of [canvas,video]) {
    const touches = new Map();
    let scrolling = false, dragging = false;
    resetTouchInputs.push(() => { touches.clear(); scrolling = dragging = false; });
    const centerY = () => [...touches.values()].reduce((sum, p) => sum + p.y, 0) / touches.size;
    target.onpointerdown=e=>{
        e.preventDefault();
        // Keep Android's IME open while interacting with the remote desktop.
        // Prevent the browser's default focus change and do not focus the surface.
        if(document.activeElement!==nativeInput) target.focus();
        target.setPointerCapture(e.pointerId);
        if(e.pointerType!=='touch') {point(e);send({type:'button',button:e.button,down:true});return;}
        touches.set(e.pointerId,{x:e.clientX,y:e.clientY,startX:e.clientX,startY:e.clientY});
        if(touches.size===1 && !scrolling) point(e);
        if(touches.size>1) {
            // Do not turn the remaining finger back into a click/drag after scrolling.
            scrolling=true;
            if(dragging) send({type:'button',button:0,down:false});
            dragging=false;
        }
    };
    target.onpointermove=e=>{
        if(e.pointerType!=='touch') {point(e);return;}
        const touch=touches.get(e.pointerId);if(!touch) return;
        e.preventDefault();
        const previousY=centerY();touch.x=e.clientX;touch.y=e.clientY;
        if(scrolling) {
            if(touches.size===2) {
                const delta=previousY-centerY();
                if(delta) send({type:'wheel',delta});
            }
            return;
        }
        // Defer the button press so placing a second finger does not click.
        if(!dragging && Math.hypot(touch.x-touch.startX,touch.y-touch.startY)>=8) {
            send({type:'button',button:0,down:true});dragging=true;
        }
        if(dragging) point(e);
    };
    target.onpointerup=e=>{
        if(e.pointerType==='touch') {
            if(touches.has(e.pointerId) && !scrolling) {
                point(e);
                if(!dragging) send({type:'button',button:0,down:true});
                send({type:'button',button:0,down:false});
            }
            touches.delete(e.pointerId);
            if(!touches.size) scrolling=dragging=false;
        } else {point(e);send({type:'button',button:e.button,down:false});}
        if(target.hasPointerCapture(e.pointerId)) target.releasePointerCapture(e.pointerId);
    };
    target.onpointercancel=()=>send({type:'release'});
    target.onlostpointercapture=e=>{if(touches.has(e.pointerId)) send({type:'release'});};
    target.oncontextmenu=e=>e.preventDefault();
    target.addEventListener('wheel',e=>{e.preventDefault();send({type:'wheel',delta:e.deltaY*(e.deltaMode===1?16:e.deltaMode===2?240:1)});},{passive:false});
}
const codes={Escape:1,Minus:12,Equal:13,Backspace:14,Tab:15,BracketLeft:26,BracketRight:27,Enter:28,ControlLeft:29,Semicolon:39,Quote:40,Backquote:41,ShiftLeft:42,Backslash:43,Comma:51,Period:52,Slash:53,ShiftRight:54,AltLeft:56,Space:57,CapsLock:58,NumLock:69,ScrollLock:70,ControlRight:97,AltRight:100,Home:102,ArrowUp:103,PageUp:104,ArrowLeft:105,ArrowRight:106,End:107,ArrowDown:108,PageDown:109,Insert:110,Delete:111,MetaLeft:125,MetaRight:126};
'1234567890'.split('').forEach((c,i)=>codes['Digit'+c]=i+2);
for(const [chars,start]of [['QWERTYUIOP',16],['ASDFGHJKL',30],['ZXCVBNM',44]])[...chars].forEach((c,i)=>codes['Key'+c]=start+i);
for(let i=1;i<=12;i++)codes['F'+i]=i<=10?58+i:76+i;
for(const target of [canvas,video])for(const type of ['keydown','keyup'])target.addEventListener(type,e=>{const code=codes[e.code];if(code){e.preventDefault();if(!e.repeat)send({type:'key',code,down:type==='keydown'})}});
const nativeInput=$('nativeInput'), nativePanel=$('nativeKeyboard');
let nativeValue='', composing=false;
let keyboardViewportHeight=0, keyboardViewportShrunk=false;
const sentinel='\u200b';
function resetNativeInput() {nativeValue='';nativeInput.value=sentinel;}
function nativeKey(code) {send({type:'key',code,down:true});send({type:'key',code,down:false});}
function commitNativeInput() {
    if(composing)return;
    const value=nativeInput.value.replace(/^\u200b/,'');
    const old=[...nativeValue], next=[...value];
    let common=0;
    while(common<old.length && common<next.length && old[common]===next[common])common++;
    for(let i=common;i<old.length;i++)nativeKey(14);
    const added=next.slice(common);
    // Keep each text message below the server input limit, including JSON escaping.
    for(let i=0;i<added.length;i+=512)send({type:'text',text:added.slice(i,i+512).join('')});
    nativeValue=value;
    if(!value || value.length>2048)resetNativeInput();
}
nativeInput.addEventListener('compositionstart',()=>{composing=true;});
nativeInput.addEventListener('compositionend',()=>{composing=false;commitNativeInput();});
nativeInput.addEventListener('input',commitNativeInput);
nativeInput.addEventListener('beforeinput',e=>{
    if(composing || e.isComposing)return;
    if(e.inputType==='insertLineBreak' || e.inputType==='insertParagraph') {
        e.preventDefault();nativeKey(28);resetNativeInput();
    } else if(e.inputType==='deleteContentBackward' && !nativeValue) {
        e.preventDefault();nativeKey(14);resetNativeInput();
    }
});
nativeInput.addEventListener('keydown',e=>{
    if(e.isComposing || e.keyCode===229)return;
    const key={Enter:28,Backspace:14,Tab:15,Escape:1,ArrowLeft:105,ArrowRight:106,ArrowUp:103,ArrowDown:108}[e.key];
    if(key){e.preventDefault();nativeKey(key);resetNativeInput();}
});
function closeNativeKeyboard() {
    commitNativeInput();nativeInput.blur();nativePanel.hidden=true;
    document.body.classList.remove('keyboard-open');keyboardViewportShrunk=false;
    $('keyboard').setAttribute('aria-expanded','false');fitCanvas();
}
$('keyboard').onclick=()=>{
    if(!nativePanel.hidden && document.activeElement===nativeInput){closeNativeKeyboard();return;}
    send({type:'release'});nativePanel.hidden=false;resetNativeInput();
    keyboardViewportHeight=window.visualViewport?.height || window.innerHeight;
    keyboardViewportShrunk=false;document.body.classList.add('keyboard-open');
    $('keyboard').setAttribute('aria-expanded','true');
    // Focus synchronously inside the tap: Android opens the user's own IME.
    nativeInput.focus({preventScroll:true});fitCanvas();
};
$('keyboard').addEventListener('pointerdown',e=>e.preventDefault());
nativeInput.addEventListener('blur',()=>document.body.classList.remove('keyboard-open'));
function fitViewport() {
    const height=window.visualViewport?.height || window.innerHeight;
    // Android Back may dismiss the IME without blurring its input.
    if(!nativePanel.hidden) {
        if(height<keyboardViewportHeight-100) keyboardViewportShrunk=true;
        if(keyboardViewportShrunk && height>=keyboardViewportHeight-80) {
            document.body.classList.remove('keyboard-open');
            keyboardViewportShrunk=false;
        }
    }
    document.body.style.height=height+'px';fitCanvas();
}
window.visualViewport?.addEventListener('resize',fitViewport);
window.addEventListener('resize',fitViewport);
fitViewport();
window.addEventListener('blur',()=>send({type:'release'}));window.addEventListener('pagehide',disconnect);document.addEventListener('visibilitychange',()=>{if(document.hidden)disconnect()});
setInterval(()=>{
    const now=performance.now();
    h2w.renderFps=(drawn-lastDrawn)*1000/(now-lastTick);
    h2w.fps=currentTransport==='webrtc'&&!video.requestVideoFrameCallback?(h2w.receivedFps||0):h2w.renderFps;lastDrawn=drawn;lastTick=now;
    $('fps').textContent=h2w.fps.toFixed(0)+' fps';send({type:'ping',at:now});
    if(currentTransport==='webrtc') collectStats();
    else send({type:'feedback',decode_ms:h2w.decodeMs,jitter_ms:0,loss:0,rtt_ms:h2w.rtt});
    const n=v=>Number(v||0).toFixed(1);
    $('metrics').textContent=[
        currentTransport==='webrtc'?'WebRTC · UDP':'WebSocket · TCP',
        h2w.fps.toFixed(0)+' fps · '+(currentTransport==='webrtc'?video.videoWidth:canvas.width)+' px',
        'Capture   '+n(h2w.captureMs)+' ms','Encode    '+n(h2w.encodeMs)+' ms',
        'Decode    '+n(h2w.decodeMs)+' ms'+(currentTransport==='webrtc'?' · '+n(h2w.receivedFps)+' fps':''),
        currentTransport==='webrtc'?'Jitter buf '+n(h2w.jitterMs)+' ms':'Draw      '+n(h2w.drawMs)+' ms',
        'RTT       '+n(h2w.rtt)+' ms',
        currentTransport==='webrtc'?'Loss      '+n(h2w.loss*100)+'%':'Frame→ACK '+n(h2w.ackMs)+' ms',
        'Bitrate cap '+n(h2w.bitrate/1e6)+' Mbps',
        'Decoder   '+h2w.decoder+(h2w.powerEfficient===true?' (efficient)':''),
        h2w.capability?'Efficiency hint '+(h2w.capability.powerEfficient?'efficient':'not efficient'):'',
    ].join('\n');
},1000);
$('statsOverlay').hidden=query.get('stats')!=='1';
$('statsToggle').onclick=()=>{$('statsOverlay').hidden=!$('statsOverlay').hidden;};
$('settingsToggle').onclick=()=>{$('settings').hidden=!$('settings').hidden;};
connect();
