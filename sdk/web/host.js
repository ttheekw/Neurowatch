/** Parent-side bridge. This allowlist, camera scope, and opaque-origin sandbox
 * are part of the security boundary; plugins never receive a generic RPC API. */
const MAX_BODY = 4 * 1024 * 1024, CHUNK = 48 * 1024;
const object = value => value && typeof value === 'object' && !Array.isArray(value);
const validPath = path => typeof path === 'string' && path.startsWith('/') && !path.startsWith('//') && path.length <= 2048 && !/[\x00-\x1f\\]/.test(path);
function decode(text) { return Uint8Array.from(atob(text),char=>char.charCodeAt(0)); }
function encode(bytes) { let text=''; for(let at=0;at<bytes.length;at+=8192) text+=String.fromCharCode(...bytes.subarray(at,at+8192)); return btoa(text); }
export function createPluginPageController({page,viewer,call,onLive=()=>{},onPlay=()=>{},onResize=()=>{},theme={}}) {
  const cameraIds = new Set(page.cameras.map(camera=>camera.id));
  const camera = params => { const id=params.camera_id; if(typeof id!=='string'||!cameraIds.has(id)) throw new Error('Choose a camera bound to this plugin and allowed for your account.');return id; };
  const manager = () => {if(viewer.role!=='admin')throw new Error('Only recorder managers can change plugin settings.');};
  let active=0;
  async function request(params) {
    if(active>=8)throw new Error('Too many page requests.');active++;
    try {
      const path=params.path??'/',method=params.method??'GET',body=params.body_b64??'';
      if(!validPath(path)||!['GET','HEAD','POST','PUT','PATCH','DELETE'].includes(method)||typeof body!=='string'||body.length>CHUNK*4/3+4)throw new Error('Invalid plugin page request.');
      if(viewer.role==='viewer'&&!['GET','HEAD'].includes(method))throw new Error('This page is read-only for viewers.');
      const meta=await call('plugins.web.request',{server:page.id,path,method,body_b64:body,content_type:String(params.content_type??'').slice(0,128)});
      if(!object(meta)||!Number.isSafeInteger(meta.size)||meta.size<0||meta.size>MAX_BODY||typeof meta.token!=='string'||!/^[a-f0-9]{64}$/.test(meta.token))throw new Error('Invalid plugin response.');
      const bytes=new Uint8Array(meta.size);
      for(let base=0;base<meta.size;base+=CHUNK*4){
        await Promise.all(Array.from({length:Math.min(4,Math.ceil((meta.size-base)/CHUNK))},async(_,index)=>{
          const offset=base+index*CHUNK;
          const chunk=await call('plugins.web.read',{server:page.id,token:meta.token,offset});
          if(!object(chunk)||chunk.offset!==offset||typeof chunk.data_b64!=='string'||chunk.data_b64.length>CHUNK*4/3+4)throw new Error('Invalid plugin response chunk.');
          const data=decode(chunk.data_b64),expected=Math.min(CHUNK,meta.size-offset);
          if(data.length!==expected)throw new Error('Incomplete plugin response.');bytes.set(data,offset);
        }));
      }
      return {status:meta.status,content_type:meta.content_type,body_b64:encode(bytes)};
    } finally {active--;}
  }
  return {async handle(method,params={}) {
    if(!object(params))throw new Error('Invalid bridge parameters.');
    switch(method){
      case 'init':return {user:viewer,cameras:page.cameras,theme,version:1};
      case 'request':return request(params);
      case 'snapshot': {const id=camera(params);const result=await call('camera.snapshot',{id});return {jpeg_b64:result.jpeg_b64};}
      case 'live.open': {const id=camera(params);await onLive(id);return {ok:true};}
      // The page's height, so the host can size its frame to it (no scrolling inside a scrolling page).
      case 'resize': {const height=Number(params.height);if(!Number.isFinite(height)||height<0)throw new Error('Invalid page height.');onResize(Math.min(Math.round(height),100000));return {ok:true};}
      case 'events.list': {const ids=params.camera_id?[camera(params)]:[...cameraIds];return call('events.list',{camera_ids:ids,source:'plugin',topic:page.topic,from_ms:Date.now()-7*86400000,limit:50});}
      case 'playback.open': {
        if(!Number.isSafeInteger(params.event_id)||params.event_id<=0)throw new Error('Invalid event.');
        const event=await call('events.get',{id:params.event_id});
        if(!cameraIds.has(event.camera_id)||event.source!=='plugin'||event.topic!==page.topic)throw new Error('This event is outside the plugin page scope.');
        await onPlay(event);return {ok:true};
      }
      case 'binding.get': {
        manager();const id=camera(params);const config=await call('plugins.get',{});
        const binding=config.bindings.find(item=>item.server===page.id&&item.camera_id===id);
        if(!binding)throw new Error('This camera is no longer bound.');return {config:binding.config};
      }
      case 'binding.save': {
        manager();const id=camera(params);if(!object(params.config)||JSON.stringify(params.config).length>65536)throw new Error('Invalid binding configuration.');
        const config=await call('plugins.get',{});
        if(!config.bindings.some(item=>item.server===page.id&&item.camera_id===id))throw new Error('This camera is no longer bound.');
        await call('plugins.set',{...config,bindings:config.bindings.map(item=>item.server===page.id&&item.camera_id===id?{...item,config:params.config}:item)});return {ok:true};
      }
      default:throw new Error('This operation is not available to plugin pages.');
    }
  }};
}
export function frameDocument(html,nonce,sdk) {
  if(!/^[a-f0-9]{64}$/.test(nonce))throw new Error('Invalid frame nonce.');
  const policy="default-src 'none'; script-src 'unsafe-inline' blob: data:; style-src 'unsafe-inline' blob:; img-src data: blob:; font-src data: blob:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'";
  return `<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${policy}"><meta name="referrer" content="no-referrer"><script data-nw-nonce="${nonce}">${sdk}</script>${html}`;
}
// A frame gets plain data: a framework's reactive proxy cannot be sent with postMessage.
const plain=value=>value===undefined?undefined:JSON.parse(JSON.stringify(value));
export function bindPluginFrame(iframe,controller,nonce) {
  let disposed=false;
  async function message(event){
    if(disposed||event.source!==iframe.contentWindow||event.origin!=='null'||!object(event.data)||event.data.nw!==1||event.data.nonce!==nonce||!Number.isSafeInteger(event.data.id))return;
    const {id,method,params}=event.data;
    try{const result=await controller.handle(method,params);if(!disposed)iframe.contentWindow?.postMessage({nw:1,nonce,id,result:plain(result)},'*');}
    catch(error){if(!disposed)iframe.contentWindow?.postMessage({nw:1,nonce,id,error:error instanceof Error?error.message:'Plugin request failed.'},'*');}
  }
  window.addEventListener('message',message);
  return ()=>{disposed=true;window.removeEventListener('message',message);};
}
