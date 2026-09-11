import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import crypto from 'node:crypto';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import WebSocket from 'ws';

async function bounded(promise,ms=8000){
 let timer;try{return await Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('test timeout')),ms)})])}finally{clearTimeout(timer)}
}

test('ordered auth, suspension closes active sockets, stale decisions and authority outage fail closed',async()=>{
 const root=await mkdtemp(join(tmpdir(),'gap-realtime-policy-'));
 let allowed=true,generation=0,unavailable=false;
 const secret='isolated-test-secret';
 const backend=http.createServer(async(req,res)=>{
  const chunks=[];for await(const c of req)chunks.push(c);
  assert.equal(req.headers.authorization,'Bearer '+secret);
  if(unavailable){res.writeHead(503);res.end('{}');return}
  const body=chunks.length?JSON.parse(Buffer.concat(chunks)):{};
  res.writeHead(200,{'content-type':'application/json'});
  res.end(JSON.stringify(req.url==='/internal/workload-policy'?{policies:Object.fromEntries(body.project_ids.map(id=>[id,{allowed,generation}]))}:{ok:true}));
 });
 backend.listen(0,'127.0.0.1');await once(backend,'listening');
 const reservation=http.createServer();reservation.listen(0,'127.0.0.1');await once(reservation,'listening');
 const port=reservation.address().port;await new Promise(r=>reservation.close(r));
 const env={...process.env,REALTIME_PORT:String(port),REALTIME_SECRET:secret,REALTIME_DB:join(root,'realtime.sqlite'),GAP_NODE_INTERNAL_URL:'http://127.0.0.1:'+backend.address().port};
 const child=spawn(process.execPath,[new URL('./server.mjs',import.meta.url).pathname],{env,stdio:'ignore'});
 const sockets=[];
 try{
  for(let i=0;i<100;i++){try{if((await fetch('http://127.0.0.1:'+port+'/health')).ok)break}catch{}await new Promise(r=>setTimeout(r,20))}
  const legacy=spawn(process.execPath,[new URL('./test.mjs',import.meta.url).pathname],{env:{...env,REALTIME_URL:'ws://127.0.0.1:'+port+'/v1/realtime'},stdio:'ignore'});
  assert.equal((await bounded(once(legacy,'exit')))[0],0,'existing pub/sub and readonly permissions still work');
  async function connect(){
   const ws=new WebSocket('ws://127.0.0.1:'+port+'/v1/realtime');sockets.push(ws);await bounded(once(ws,'open'));
   const claims=Buffer.from(JSON.stringify({project_id:'prj_test',channels:['test'],exp:Math.floor(Date.now()/1000)+60,jti:'test'})).toString('base64url');
   const response=once(ws,'message');ws.send(JSON.stringify({action:'authenticate',token:claims+'.'+crypto.createHmac('sha256',secret).update(claims).digest('hex')}));
   return [ws,JSON.parse((await bounded(response))[0].toString())];
  }
  const [first,auth]=await connect();assert.equal(auth.type,'authenticated');
  const closed=once(first,'close');allowed=false;generation=1;await bounded(closed);
  const [denied,error]=await connect();assert.equal(error.type,'error');denied.terminate();
  allowed=true;generation=2;const [restored,ok]=await connect();assert.equal(ok.type,'authenticated');restored.terminate();
  generation=1;const [stale,staleError]=await connect();assert.equal(staleError.type,'error');stale.terminate();
  generation=3;const [live,liveAuth]=await connect();assert.equal(liveAuth.type,'authenticated');
  const disconnected=once(live,'close');unavailable=true;await bounded(disconnected);
 }finally{
  for(const ws of sockets)ws.terminate();
  const exited=once(child,'exit');child.kill();await exited;
  backend.closeAllConnections();await new Promise(r=>backend.close(r));await rm(root,{recursive:true,force:true});
 }
});
