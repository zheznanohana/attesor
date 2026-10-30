//! Bounded scalar SSA slices. No guest memory, calls, labels or approximations.
//! A final NaN guard is sufficient only when its ancestry covers every live
//! written V version; overwritten/unobserved intermediate versions are dead.
use super::*;

const MAX:usize=5;
const TEMPS:[AsmRegisterXmm;MAX]=[xmm0,xmm1,xmm2,xmm3,xmm4];

#[derive(Clone,Copy,Debug)]
struct Source { guest:u32, producer:Option<usize> }
#[derive(Clone,Copy,Debug)]
struct Node { code:u32,dest:u32,n:Source,m:Option<Source>,ancestry:u8 }
#[derive(Clone,Debug)]
struct Trace { double:bool,nodes:Vec<Node>,latest:[Option<usize>;32] }

fn decode(w:u32)->Option<(bool,u32,u32,u32,Option<u32>)> {
    let ty=bits(w,23,22);if ty>1 {return None;}
    let code=if w&0xff200c00==0x1e200800&&bits(w,15,12)<=3 {bits(w,15,12)}
        else if w&0xff3ffc00==0x1e21c000 {4}else {return None;};
    Some((ty==1,code,w&31,bits(w,9,5),(code!=4).then(||bits(w,20,16))))
}

fn plan(words:impl IntoIterator<Item=u32>,room:usize,cut:impl Fn(usize)->bool)->Option<Trace> {
    let mut trace=Trace{double:false,nodes:Vec::new(),latest:[None;32]};let mut best=None;
    for (i,w) in words.into_iter().take(MAX.min(room)).enumerate() {
        if i!=0&&cut(i) {break;}
        let Some((double,code,dest,n,m))=decode(w) else {break;};
        if i!=0&&double!=trace.double {break;}trace.double=double;
        let n=Source{guest:n,producer:trace.latest[n as usize]};
        let m=m.map(|r|Source{guest:r,producer:trace.latest[r as usize]});
        let mut ancestry=1u8<<i;
        for s in [Some(n),m].into_iter().flatten() {if let Some(p)=s.producer {ancestry|=trace.nodes[p].ancestry;}}
        trace.nodes.push(Node{code,dest,n,m,ancestry});trace.latest[dest as usize]=Some(i);
        if i>=2&&trace.latest.iter().flatten().all(|&p|ancestry&(1<<p)!=0) {best=Some(trace.clone());}
    }
    best
}

impl E<'_> {
    pub(super) fn scalar_trace(&mut self,insn:u32,room:usize)->bool {
        if !self.u.vtrace||!self.u.vchain||!has_avx()||disabled_classes()!=0 {return false;}
        let Some(region)=self.u.region else {return false;};
        let Some(trace)=plan(std::iter::once(insn).chain(self.u.look.iter().copied()),room,
            |i|self.u.labels.contains_key(&(self.pc+i as u64*4))) else {return false;};
        if trace.nodes.iter().any(|n|[Some(n.dest),Some(n.n.guest),n.m.map(|s|s.guest)]
            .into_iter().flatten().any(|r|region.vslot(r).is_none())) {return false;}
        // Capture ORIGINAL mapped locations before any result is committed.
        let originals:Vec<_>=trace.nodes.iter().map(|node|
            (self.scalar_source(node.n.guest),node.m.map(|s|self.scalar_source(s.guest)))).collect();
        let known=self.u.vzero[trace.double as usize];
        for(i,node)in trace.nodes.iter().enumerate() {
            let (ns,ms)=originals[i];
            let nr=node.n.producer.map_or_else(||ns.cached.unwrap(),|p|TEMPS[p]);
            let nzero=node.n.producer.is_some()||known&(1<<node.n.guest)!=0;
            let mr=node.m.map_or(nr,|s|s.producer.map_or_else(||ms.unwrap().cached.unwrap(),|p|TEMPS[p]));
            let mzero=node.m.is_some_and(|s|s.producer.is_some()||known&(1<<s.guest)!=0);
            if nzero {self.fp_register(node.code,trace.double,TEMPS[i],nr,mr);}
            else if matches!(node.code,0|2)&&mzero {self.fp_register(node.code,trace.double,TEMPS[i],mr,nr);}
            else {
                self.fld_source(xmm7,ns,trace.double);
                self.fp_register(node.code,trace.double,TEMPS[i],xmm7,if node.code==4 {xmm7}else {mr});
            }
        }
        let root=TEMPS[trace.nodes.len()-1];let cold=self.u.a.create_label();
        if trace.double {self.u.a.ucomisd(root,root).unwrap();}else {self.u.a.ucomiss(root,root).unwrap();}
        self.u.a.jp(cold).unwrap();
        let replay=trace.clone();
        self.cold_path(cold,move|e| {
            // Cold-only canonical scratch. Prior producers have already stored
            // their strict result; untouched original inputs remain in mapped V.
            for(i,node)in replay.nodes.iter().enumerate() {
                let resolve=|s:Source,original:FpSource|if s.producer.is_some()
                    {FpSource{cached:None,offset:voff(s.guest)}}else {original};
                let ns=resolve(node.n,originals[i].0);
                let ms=node.m.map(|s|resolve(s,originals[i].1.unwrap()));
                e.fld_source(xmm0,ns,replay.double);
                if let Some(src)=ms {e.fld_source(xmm2,src,replay.double);e.fp_register(node.code,replay.double,xmm0,xmm0,xmm2);}
                else {e.fp_register(4,replay.double,xmm0,xmm0,xmm0);}
                e.scalar_nan(replay.double,ns,ms);
                e.u.a.movdqu(xmmword_ptr(r15+voff(node.dest)),xmm0).unwrap();
            }
            // Replay clobbered scratch temporaries: restore only live versions,
            // in distinct destinations, before joining the common commit tail.
            for(r,p)in replay.latest.iter().enumerate() {if let Some(p)=p {
                e.fld_source(TEMPS[*p],FpSource{cached:None,offset:voff(r as u32)},replay.double);
            }}
        });
        for(r,p)in trace.latest.iter().enumerate() {if let Some(p)=p {
            self.store_v(r as u32,TEMPS[*p]);self.scalar_zero(r as u32,trace.double);
        }}
        self.u.skip_next=true;self.u.skip_more=(trace.nodes.len()-2)as u8;
        self.u.insns_translated+=(trace.nodes.len()-1)as u64;true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vtrace_planner_requires_all_live_outputs_and_honest_boundaries() {
        // Independent squares followed by a sum: root covers both live outputs.
        let w=[0x1e610822,0x1e600803,0x1e632842];
        assert_eq!(plan(w,5,|_|false).unwrap().nodes.len(),3);
        assert!(plan(w,2,|_|false).is_none());assert!(plan(w,5,|i|i==2).is_none());
        for stop in [0xfd400000,0xd4001001,0x94000000,0x1e216800] {
            assert!(plan([w[0],stop,w[2]],5,|_|false).is_none());
        }
        assert!(plan([w[0],w[1],0x1e612800],5,|_|false).is_none()); // unrelated root
        assert!(plan([w[0],w[1],w[2]^(1<<22)],5,|_|false).is_none());
    }
}
