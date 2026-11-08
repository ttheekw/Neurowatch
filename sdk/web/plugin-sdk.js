/* NeuroWatch plugin UI SDK v1. Runs inside an opaque-origin sandbox. */
(()=>{
  'use strict';
  const nonce=document.currentScript?.dataset.nwNonce;
  if(!/^[a-f0-9]{64}$/.test(nonce??''))throw new Error('Plugin SDK requires the NeuroWatch host.');
  let next=1,context=null,selected=null;const pending=new Map(),assets=[];let assetBytes=0;
  function call(method,params={}){
    if(pending.size>=32)return Promise.reject(new Error('Too many plugin UI requests.'));
    const id=next++;
    return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{pending.delete(id);reject(new Error('The recorder request timed out.'));},30000);
      pending.set(id,{resolve,reject,timer});parent.postMessage({nw:1,nonce,id,method,params},'*');
    });
  }
  addEventListener('message',event=>{
    const value=event.data;
    if(event.source!==parent||!value||value.nw!==1||value.nonce!==nonce||!Number.isSafeInteger(value.id))return;
    const request=pending.get(value.id);if(!request)return;clearTimeout(request.timer);pending.delete(value.id);
    if(typeof value.error==='string')request.reject(new Error(value.error));else request.resolve(value.result);
  });
  function bytes(text){return Uint8Array.from(atob(text),char=>char.charCodeAt(0));}
  function base64(value){let text='';for(let at=0;at<value.length;at+=8192)text+=String.fromCharCode(...value.subarray(at,at+8192));return btoa(text);}
  async function fetchPage(path,options={}){
    let body=new Uint8Array();
    if(options.body!=null){body=typeof options.body==='string'?new TextEncoder().encode(options.body):new Uint8Array(options.body instanceof Blob?await options.body.arrayBuffer():options.body);}
    if(body.length>48*1024)throw new Error('Plugin requests are limited to 48 KiB.');
    const response=await call('request',{path:String(path),method:options.method??'GET',content_type:new Headers(options.headers).get('content-type')??'',body_b64:base64(body)});
    return new Response(bytes(response.body_b64),{status:response.status,headers:{'Content-Type':response.content_type}});
  }
  const ready=call('init').then(value=>{
    context=value;selected=value.cameras[0]?.id??null;
    for(const[key,color]of Object.entries(value.theme??{}))if(/^--nw-[a-z-]+$/.test(key))document.documentElement.style.setProperty(key,String(color));
    dispatchEvent(new CustomEvent('nw:ready',{detail:value}));return value;
  });
  const sdk={version:1,ready,call,fetch:fetchPage,get camera(){return selected;},selectCamera(id){if(!context?.cameras.some(camera=>camera.id===id))throw new Error('Camera not allowed.');selected=id;dispatchEvent(new CustomEvent('nw:camera',{detail:id}));},async asset(path){const response=await fetchPage(path);if(!response.ok)throw new Error(`Asset returned ${response.status}`);const blob=await response.blob();while(assets.length&&(assetBytes+blob.size>16*1024*1024||assets.length>=64)){const old=assets.shift();URL.revokeObjectURL(old.url);assetBytes-=old.size;}const url=URL.createObjectURL(blob);assets.push({url,size:blob.size});assetBytes+=blob.size;return url;}};
  Object.defineProperty(window,'NeuroWatch',{value:Object.freeze(sdk)});
  // Tell the host how tall the page is, so its frame fits it instead of scrolling inside the portal's page.
  let reported=0,queued=false;
  const measure=()=>{queued=false;const body=document.body;if(!body)return;const height=Math.ceil(body.getBoundingClientRect().bottom+window.scrollY+(parseFloat(getComputedStyle(body).marginBottom)||0));if(Math.abs(height-reported)<4)return;reported=height;call('resize',{height}).catch(()=>{});};
  const remeasure=()=>{if(!queued){queued=true;requestAnimationFrame(measure);}};
  ready.then(()=>{if(document.body)new ResizeObserver(remeasure).observe(document.body);remeasure();}).catch(()=>{});
  window.fetch=fetchPage;
  const style=document.createElement('style');style.textContent='nw-camera-picker,nw-live-tile,nw-zone-editor,nw-line-editor,nw-event-list{display:block;font:14px system-ui,sans-serif}nw-camera-picker select,nw-zone-editor select,nw-line-editor select,nw-zone-editor input,nw-line-editor input{font:inherit;min-height:40px;padding:6px 10px;background:var(--nw-surface,#151a28);color:var(--nw-text,#e2e8f0);border:1px solid #64748b88;border-radius:6px}nw-zone-editor button,nw-line-editor button,nw-live-tile button,nw-event-list button{font:inherit;min-height:40px;margin:4px 6px 4px 0;padding:7px 12px;border:1px solid #64748b88;border-radius:6px;background:var(--nw-surface,#151a28);color:var(--nw-text,#e2e8f0);cursor:pointer}nw-zone-editor canvas,nw-line-editor canvas{display:block;width:100%;max-width:960px;background:#080b12;border-radius:8px;margin:12px 0;touch-action:none}nw-event-list ul{list-style:none;padding:0}nw-event-list li{padding:8px 0;border-bottom:1px solid #64748b44}nw-event-list button{width:100%;text-align:left}nw-live-tile p{color:var(--nw-muted,#94a3b8)}';document.head.append(style);
  const el=(tag,text)=>{const item=document.createElement(tag);if(text!=null)item.textContent=text;return item;};
  class CameraPicker extends HTMLElement{
    connectedCallback(){ready.then(value=>{if(!this.isConnected)return;this.replaceChildren();const label=el('label',this.getAttribute('label')??'Camera '),select=el('select');select.setAttribute('aria-label','Plugin camera');for(const camera of value.cameras){const option=el('option',camera.name);option.value=camera.id;select.append(option);}select.value=selected??'';select.addEventListener('change',()=>sdk.selectCamera(select.value));label.append(select);this.append(label);});}
  }
  class LiveTile extends HTMLElement{
    connectedCallback(){this.update=()=>{const camera=context?.cameras.find(camera=>camera.id===(this.getAttribute('camera')??selected));this.replaceChildren();if(!camera){this.append(el('p','No camera available.'));return;}const button=el('button',`Open live view · ${camera.name}`);button.type='button';button.onclick=async()=>{try{await call('live.open',{camera_id:camera.id});this.note.textContent='The camera is open in the host view beside this page.';}catch(error){this.note.textContent=error.message;}};this.note=el('p','Live view uses the recorder’s normal viewer.');this.append(button,this.note);};addEventListener('nw:camera',this.update);ready.then(this.update);}
    disconnectedCallback(){removeEventListener('nw:camera',this.update);}
  }
  class EventList extends HTMLElement{
    connectedCallback(){this.epoch=0;this.update=()=>this.load();addEventListener('nw:camera',this.update);ready.then(this.update);this.timer=setInterval(this.update,10000);}
    disconnectedCallback(){this.epoch++;removeEventListener('nw:camera',this.update);clearInterval(this.timer);}
    async load(){const epoch=++this.epoch;try{const camera=this.getAttribute('camera')??selected;const response=await call('events.list',camera?{camera_id:camera}:{});if(epoch!==this.epoch||!this.isConnected)return;this.replaceChildren();const list=el('ul');for(const event of response.events??[]){const row=el('li'),button=el('button',`${new Date(event.start_ms).toLocaleString()} · ${event.kind}${event.end_ms==null?' · ongoing':''}`);button.type='button';button.onclick=()=>call('playback.open',{event_id:event.id}).catch(error=>{this.prepend(el('p',error.message));});row.append(button);list.append(row);}this.append(list);if(!list.childElementCount)this.append(el('p','No events in the last seven days.'));}catch(error){if(epoch===this.epoch)this.replaceChildren(el('p',error.message));}}
  }
  class Editor extends HTMLElement{
    connectedCallback(){this.epoch=0;this.update=()=>this.load();addEventListener('nw:camera',this.update);ready.then(this.update);}
    disconnectedCallback(){this.epoch++;removeEventListener('nw:camera',this.update);}
    async load(){
      const epoch=++this.epoch;this.replaceChildren();if(context?.user.role!=='admin'){this.append(el('p','Editing is available to recorder managers.'));return;}
      const camera=this.getAttribute('camera')??selected;if(!camera){this.append(el('p','Choose a camera.'));return;}
      try{
        const saved=await call('binding.get',{camera_id:camera});if(epoch!==this.epoch||!this.isConnected)return;
        this.camera=camera;this.config=structuredClone(saved.config);this.key=this.getAttribute('config-key')??(this.localName==='nw-line-editor'?'lines':'zones');this.lines=this.localName==='nw-line-editor';
        this.items=Array.isArray(this.config[this.key])?structuredClone(this.config[this.key]):[];this.index=0;this.image=null;
        const tools=el('div');this.choose=el('select');this.choose.setAttribute('aria-label',this.lines?'Analysis line':'Analysis zone');this.choose.onchange=()=>{this.index=Number(this.choose.value);this.render();};tools.append(this.choose);
        const button=(label,handler)=>{const button=el('button',label);button.type='button';button.onclick=handler;tools.append(button);};
        button(this.lines?'New line':'New zone',()=>{if(this.items.length>=32)return;const id=Array.from(crypto.getRandomValues(new Uint8Array(8)),byte=>byte.toString(16).padStart(2,'0')).join('');this.items.push({id,name:`${this.lines?'Line':'Zone'} ${this.items.length+1}`,kind:'include',points:[]});this.index=this.items.length-1;this.render();});
        button('Undo point',()=>{this.items[this.index]?.points?.pop();this.paint();});button('Remove',()=>{this.items.splice(this.index,1);this.index=Math.max(0,this.index-1);this.render();});
        this.name=el('input');this.name.setAttribute('aria-label','Name');this.name.maxLength=64;this.name.oninput=()=>{if(this.items[this.index])this.items[this.index].name=this.name.value;};
        this.kind=el('select');this.kind.setAttribute('aria-label','Zone type');for(const[k,label]of [['include','Watch this area'],['exclude','Ignore this area']]){const option=el('option',label);option.value=k;this.kind.append(option);}this.kind.onchange=()=>{if(this.items[this.index])this.items[this.index].kind=this.kind.value;this.paint();};
        this.canvas=el('canvas');this.canvas.width=960;this.canvas.height=540;this.canvas.setAttribute('aria-label',this.lines?'Click two points for the line':'Click at least three points for a zone');this.canvas.onclick=event=>{const item=this.items[this.index];if(!item)return;item.points??=[];if(item.points.length>=(this.lines?2:64))return;const rect=this.canvas.getBoundingClientRect(),image=this.rect(),x=(event.clientX-rect.left)*960/rect.width,y=(event.clientY-rect.top)*540/rect.height;if(x<image.x||x>image.x+image.w||y<image.y||y>image.y+image.h)return;item.points.push([(x-image.x)/image.w,(y-image.y)/image.h]);this.paint();};
        this.note=el('p','Choose New, then click points on the camera picture.');const save=el('button','Save');save.type='button';save.onclick=async()=>{if(this.items.some(item=>!Array.isArray(item.points)||item.points.length<(this.lines?2:3))){this.note.textContent='Finish each shape before saving.';return;}try{await call('binding.save',{camera_id:camera,config:{...this.config,[this.key]:this.items}});this.note.textContent='Saved on the recorder.';}catch(error){this.note.textContent=error.message;}};
        this.append(tools,this.name);if(!this.lines)this.append(this.kind);this.append(this.canvas,this.note,save);this.render();
        try{const snapshot=await call('snapshot',{camera_id:camera});if(epoch!==this.epoch)return;const image=new Image();image.onload=()=>{if(epoch===this.epoch){this.image=image;this.paint();}};image.src=`data:image/jpeg;base64,${snapshot.jpeg_b64}`;}catch{if(epoch===this.epoch)this.note.textContent='Snapshot unavailable. Points use normalized picture coordinates.';}
      }catch(error){if(epoch===this.epoch)this.replaceChildren(el('p',error.message));}
    }
    rect(){const width=this.image?.naturalWidth??16,height=this.image?.naturalHeight??9,scale=Math.min(960/width,540/height);return{x:(960-width*scale)/2,y:(540-height*scale)/2,w:width*scale,h:height*scale};}
    render(){this.choose.replaceChildren();this.items.forEach((item,index)=>{const option=el('option',item.name??`Shape ${index+1}`);option.value=String(index);this.choose.append(option);});this.choose.value=String(this.index);this.name.value=this.items[this.index]?.name??'';this.kind.value=this.items[this.index]?.kind??'include';this.paint();}
    paint(){if(!this.canvas)return;const draw=this.canvas.getContext('2d'),rect=this.rect();draw.clearRect(0,0,960,540);if(this.image)draw.drawImage(this.image,rect.x,rect.y,rect.w,rect.h);this.items.forEach((item,index)=>{const points=Array.isArray(item.points)?item.points.slice(0,64).filter(point=>Array.isArray(point)&&point.length===2&&point.every(value=>Number.isFinite(value)&&value>=0&&value<=1)):[];if(!points.length)return;draw.strokeStyle=item.kind==='exclude'?'#ef4444':'#f59e0b';draw.lineWidth=index===this.index?4:2;draw.beginPath();points.forEach(([x,y],at)=>{draw[at?'lineTo':'moveTo'](rect.x+x*rect.w,rect.y+y*rect.h);});if(!this.lines&&points.length>=3)draw.closePath();draw.stroke();for(const[x,y]of points){draw.beginPath();draw.arc(rect.x+x*rect.w,rect.y+y*rect.h,5,0,Math.PI*2);draw.fillStyle=draw.strokeStyle;draw.fill();}});}
  }
  customElements.define('nw-camera-picker',CameraPicker);customElements.define('nw-live-tile',LiveTile);customElements.define('nw-event-list',EventList);customElements.define('nw-zone-editor',class extends Editor{});customElements.define('nw-line-editor',class extends Editor{});
})();
