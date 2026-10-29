function t(name, f){ const s=Date.now(); const r=f(); console.log(name.padEnd(10), String(Date.now()-s).padStart(6),'ms', r); }
t('loops', ()=>{ let s=0; for(let i=0;i<3e7;i++) s=(s+i*7)%1000003; return s; });
t('objects', ()=>{ let a=[]; for(let i=0;i<4e5;i++) a.push({id:i, name:'n'+i, v:i*1.5}); let s=0; for(const o of a) s+=o.v; return s|0; });
t('json', ()=>{ const o={a:[...Array(2000).keys()].map(i=>({i, s:'x'+i, f:[1,2,3]}))}; let n=0; for(let k=0;k<40;k++) n+=JSON.parse(JSON.stringify(o)).a.length; return n; });
t('regex', ()=>{ const s=('foo bar baz 12345 qux ').repeat(20000); let n=0; for(let k=0;k<20;k++) n+=(s.match(/\b\d+\b/g)||[]).length; return n; });
t('strings', ()=>{ let s=''; for(let i=0;i<2e5;i++) s+=String.fromCharCode(65+i%26); let h=0; for(let i=0;i<s.length;i++) h=(h*31+s.charCodeAt(i))|0; return h; });
t('closures', ()=>{ const fs=[]; for(let i=0;i<1000;i++) fs.push(x=>x+i); let s=0; for(let k=0;k<3000;k++) for(const f of fs) s=f(s)&0xffff; return s; });
t('sort', ()=>{ const a=[]; let x=1; for(let i=0;i<3e5;i++){x=(x*1103515245+12345)&0x7fffffff; a.push(x);} a.sort((p,q)=>p-q); return a[1000]; });
