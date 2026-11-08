export interface PluginPage { id: string; topic: string; name: string; title: string; entry: string; cameras: {id:string;name:string}[] }
export interface PluginViewer { user: string; role: 'admin'|'operator'|'viewer' }
export interface PluginPageController { handle(method:string,params?:Record<string,unknown>):Promise<unknown> }
export function createPluginPageController(options:{page:PluginPage;viewer:PluginViewer;call:(method:string,params:Record<string,unknown>)=>Promise<unknown>;onLive?:(camera:string)=>void|Promise<void>;onPlay?:(event:{id:number;camera_id:string;start_ms:number})=>void|Promise<void>;onResize?:(height:number)=>void;theme?:Record<string,string>}):PluginPageController;
export function frameDocument(html:string,nonce:string,sdk:string):string;
export function bindPluginFrame(iframe:HTMLIFrameElement,controller:PluginPageController,nonce:string):()=>void;
