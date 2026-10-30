//! Guarded read-only base shadow for a bounded scalar FP self-loop.
//! Architectural GPRs retain their original tagged values. Only a free fixed-map
//! slot holds the masked shadow, and the complete EA (including last byte) is
//! proven not to wrap bit56 for every specialized iteration.
use super::*;
#[derive(Clone,Copy,Debug)]
pub(super) struct Plan {pub at:u64,pub word:u32,pub rn:u32,pub rm:u32,pub scale:u32,pub step:u32,pub bound:u32,pub span:u64}

pub(super) fn plan(pc:u64,fp:fp_flags::Plan,read:&dyn Fn(u64)->Option<u32>)->Option<Plan> {
    if !fp.sf||fp.imm<=0||read(fp.cmp_pc.checked_add(4)?)?&0xff00001f!=0x54000001 {return None;}
    let mut chosen=None;let mut writes=Vec::new();let mut at=pc;
    while at<fp.cmp_pc {
        let w=read(at)?;
        let scalar=(w&0xff200c00==0x1e200800&&(w>>12)&15<=3||w&0xff3ffc00==0x1e21c000)&&bits(w,23,22)<=1;
        if scalar {}
        else if (w>>25)&5==4 {
            // No GP memory, literal, writeback, pair or vector memory here.
            let reg=w&0x3b200c00==0x38200800;
            if w&(1<<26)==0||w>>30<2||bits(w,23,22)>1||!(reg||w&0x3b000000==0x39000000) {return None;}
            if chosen.is_none()&&reg&&bits(w,15,13)==3&&bits(w,23,22)==1&&bits(w,20,16)==fp.rn {
                let rn=bits(w,9,5);let rm=bits(w,20,16);
                if rn!=31&&rn!=rm {chosen=Some((at,w,rn,rm,if bit(w,12)!=0 {w>>30}else {0}));}
            }
        } else if w&0xff800000==0x91000000 {
            // The only GP writers admitted are exact nonflag ADD Xn,Xn,#imm.
            let n=w&31;if n==31||bits(w,9,5)!=n {return None;}
            let delta=bits(w,21,10)<<if bit(w,22)!=0 {12}else {0};
            writes.push((at,n,delta));
        } else {return None;}
        at=at.checked_add(4)?;
    }
    let (at,word,rn,rm,scale)=chosen?;
    if writes.iter().any(|&(_,n,_)|n==rn) {return None;}
    let mut index_writes=writes.iter().filter(|&&(_,n,_)|n==rm);
    let &(update,_,step)=index_writes.next()?;
    if index_writes.next().is_some()||update<=at||!step.is_power_of_two() {return None;}
    let bound=fp.imm as u32;
    if bound<step||bound%step!=0 {return None;}
    let span=((bound-step)as u64).checked_shl(scale)?+((1u64<<(word>>30))-1);
    Some(Plan{at,word,rn,rm,scale,step,bound,span})
}

pub(super) fn enter(u:&mut Unit,p:Plan,region:LoopRa,base:usize,index:usize,shadow:usize) {
    let mut bad=u.a.create_label();let dst=LOOPREGS[shadow];let idx=LOOPREGS[index];
    u.a.cmp(idx,(p.bound-p.step)as i32).unwrap();u.a.ja(bad).unwrap();
    if p.step!=1 {u.a.test(idx,(p.step-1)as i32).unwrap();u.a.jnz(bad).unwrap();}
    u.a.mov(dst,LOOPREGS[base]).unwrap();u.a.mov(rax,0x00ff_ffff_ffff_ffffu64).unwrap();u.a.and(dst,rax).unwrap();
    u.a.mov(rax,0x00ff_ffff_ffff_ffffu64-p.span).unwrap();u.a.cmp(dst,rax).unwrap();u.a.ja(bad).unwrap();
    u.cold_paths.push(Box::new(move |u| {u.a.set_label(&mut bad).unwrap();fp_flags::reject(u,region.pc,region,None);}));
}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn fpflags_addrhoist_proof_boundaries() {
        let good=[0xfc6a6801,0xfd400022,0x1e620821,0x1e612800,0x9100214a,0xf100615f,0x54ffff41];
        let check=|w:&[u32]| {let read=|p|w.get((p/4)as usize).copied();fp_flags::plan(0,&read,&|_|false).and_then(|fp|plan(0,fp,&read))};
        let p=check(&good).unwrap();assert_eq!((p.rn,p.rm,p.scale,p.step,p.bound,p.span),(0,10,0,8,24,23));
        for (i,bad) in [(6,0x54ffff40),(6,0x54ffff4a),(0,0xfd400001),(0,0xfc6a4801),(0,0xfc6a6941),(1,0x91002000),(1,0xaa0103e1),(4,0x9100014a),(4,0x91000d4a),(4,0x9100216a),(5,0x7100615f),(5,0xf100655f)] {
            let mut w=good;w[i]=bad;assert!(check(&w).is_none(),"at={i} {bad:x}");
        }
        let mut w=good;w[1]=0x9100214a;assert!(check(&w).is_none());
    }
}
