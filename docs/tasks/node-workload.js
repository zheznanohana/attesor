const fs=require('fs'),path=require('path'),http=require('http');
const res=process.env.RES, asar=res+'/app.asar';
let t=Date.now(), n=0;
const walk=(d,depth)=>{ for (const f of fs.readdirSync(d)) { const p=path.join(d,f); const s=fs.statSync(p); n++; if (s.isDirectory()) { if (depth<6) walk(p,depth+1); } else if (s.size<65536) fs.readFileSync(p); } };
walk(asar,0); walk(res+'/app.asar.unpacked/dsh/node_modules',0);
console.log('fs entries', n, 'ms', Date.now()-t);
const srv=http.createServer((q,r)=>r.end('x'.repeat(2048))).listen(0,()=>{
  const port=srv.address().port; let left=3000, t2=Date.now();
  const one=()=>http.get({port,path:'/',agent:false},r=>{r.resume();r.on('end',()=>{ if(--left) one(); else { console.log('http 3000 req ms', Date.now()-t2); srv.close(); } });});
  for (let i=0;i<4;i++) { left--; one(); }
});
