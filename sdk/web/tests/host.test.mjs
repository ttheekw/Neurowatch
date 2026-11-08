import test from 'node:test';
import assert from 'node:assert/strict';
import { createPluginPageController, frameDocument } from '../host.js';
const page={id:'motion',topic:'nw.motion',cameras:[{id:'allowed',name:'Gate'}]};
test('plugin bridge rejects arbitrary RPCs and other cameras before transport',async()=>{
  const calls=[];const host=createPluginPageController({page,viewer:{user:'reader',role:'viewer'},call:async(method,params)=>{calls.push([method,params]);return {};}});
  await assert.rejects(host.handle('config.get',{}));
  await assert.rejects(host.handle('snapshot',{camera_id:'other'}));
  await assert.rejects(host.handle('binding.save',{camera_id:'allowed',config:{}}));
  assert.equal(calls.length,0);
  await host.handle('events.list',{camera_id:'allowed'});
  assert.equal(calls[0][0],'events.list');assert.deepEqual(calls[0][1].camera_ids,['allowed']);assert.equal(calls[0][1].topic,'nw.motion');assert.equal(calls[0][1].source,'plugin');
});
test('page fetches read bounded chunks and never pass arbitrary headers',async()=>{
  const calls=[];
  const host=createPluginPageController({page,viewer:{user:'owner',role:'admin'},call:async(method,params)=>{
    calls.push([method,params]);return method==='plugins.web.request'?{token:'a'.repeat(64),size:5,status:200,content_type:'text/html'}:{offset:0,data_b64:btoa('hello'),eof:true};
  }});
  const response=await host.handle('request',{path:'/',method:'GET',headers:{Cookie:'private'}});
  assert.equal(atob(response.body_b64),'hello');assert.equal(calls[0][1].headers,undefined);assert.equal(calls[0][1].server,'motion');
  await assert.rejects(host.handle('request',{path:'https://other.example'}));
});
test('iframe document enforces an opaque origin and blocks direct network requests',()=>{
  const html=frameDocument('<h1>Plugin</h1>','a'.repeat(64),'window.sdk=true;');
  assert.match(html,/connect-src 'none'/);assert.match(html,/form-action 'none'/);assert.match(html,/data-nw-nonce/);assert.match(html,/<h1>Plugin<\/h1>/);
});
test('a page reports its height to the host and nothing else happens',async()=>{
  const calls=[],heights=[];const host=createPluginPageController({page,viewer:{user:'reader',role:'viewer'},call:async(method)=>{calls.push(method);return {};},onResize:height=>heights.push(height)});
  assert.deepEqual(await host.handle('resize',{height:812.4}),{ok:true});
  await host.handle('resize',{height:1e9});
  await assert.rejects(host.handle('resize',{height:'tall'}));
  await assert.rejects(host.handle('resize',{height:-1}));
  assert.deepEqual(heights,[812,100000]);assert.deepEqual(calls,[]);
});
