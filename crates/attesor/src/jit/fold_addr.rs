//! Loop versioning: a full-EA low56 range proof selects folded x86 addressing.
//! Tagged/unusual inputs keep the original native TBI path, never interpreter
//! peeling. Both private entries use the SAME raw architectural assignment.
use super::*;
const MASK:u64=0x00ff_ffff_ffff_ffff;
#[derive(Clone,Copy)]
pub(super) struct Plan {words:[u32;4],base:u32,index:u32,ptr:u32,step:u32,stride:u32,bound:u32,scale:u32,offset:u32,double:bool,span_a:u64}
fn add(w:u32,n:u32)->Option<u32> {(w&0xff800000==0x91000000&&w&31==n&&bits(w,9,5)==n).then(||bits(w,21,10)<<if bit(w,22)!=0 {12}else {0})}
pub(super) fn plan(u:&Unit,pc:u64,fp:fp_flags::Plan,r:LoopRa,read:&dyn Fn(u64)->Option<u32>)->Option<Plan> {
    pc.checked_add(32)?;
    if !u.foldaddr||fp.cmp_pc!=pc+24||!fp.sf||fp.imm<=0||!fp_flags::read_only_mode(pc,fp,read) {return None;}
    let mut w=[0;8];for(i,v)in w.iter_mut().enumerate(){*v=read(pc+i as u64*4)?;}
    if w[7]!=0x54ffff21||w[0]&0x3b200c00!=0x38200800||w[1]&0x3b000000!=0x39000000 {return None;}
    if [w[0],w[1]].iter().any(|&v|bit(v,26)==0||v>>30<2||bits(v,23,22)!=1)||w[0]>>30!=w[1]>>30||bits(w[0],15,13)!=3 {return None;}
    let bound=bits(w[6],21,10)<<if bit(w[6],22)!=0 {12}else {0};
    if w[6]&0xff80001f!=0xf100001f||bits(w[6],9,5)!=fp.rn||bound as i32!=fp.imm {return None;}
    let double=w[0]>>30==3;let ty=double as u32;let(t,m,d)=(w[0]&31,w[1]&31,w[3]&31);
    if t==m||t==d||m==d||w[2]&0xff20fc00!=0x1e200800||w[3]&0xff20fc00!=0x1e202800||bits(w[2],23,22)!=ty||bits(w[3],23,22)!=ty
        ||w[2]&31!=t||bits(w[2],9,5)!=t||bits(w[2],20,16)!=m {return None;}
    let(an,am)=(bits(w[3],9,5),bits(w[3],20,16));if !((an==d&&am==t)||(an==t&&am==d)) {return None;}
    let(base,index,ptr)=(bits(w[0],9,5),bits(w[0],20,16),bits(w[1],9,5));
    if [base,index,ptr].contains(&31)||base==index||base==ptr||index==ptr||fp.rn!=index {return None;}
    let step=add(w[4],index)?;let stride=add(w[5],ptr)?;
    if !step.is_power_of_two()||bound<step||bound%step!=0 {return None;}
    for n in [base,index,ptr] {r.slot(n)?;}for n in [t,m,d] {r.vslot(n)?;}
    let scale=if bit(w[0],12)!=0 {w[0]>>30}else {0};let offset=bits(w[1],21,10)<<(w[1]>>30);let bytes=1u64<<(w[0]>>30);
    let span_a=((bound-step)as u64).checked_shl(scale)?.checked_add(bytes-1)?;
    // The emitted remaining-trip arithmetic cannot overflow or underflow MASK.
    (bound as u64).checked_mul(stride as u64)?.checked_add(offset as u64+bytes-1).filter(|&v|v<=MASK)?;
    Some(Plan{words:[w[0],w[1],w[2],w[3]],base,index,ptr,step,stride,bound,scale,offset,double,span_a})
}
fn direct_load(e:&mut E<'_>,word:u32,double:bool,mem:AsmMemoryOperand) {
    let dst=VREGS[e.u.region.unwrap().vslot(word&31).unwrap()];
    if high_xmm(dst) {if double {e.u.a.evex().vmovq(dst,mem).unwrap();}else {e.u.a.evex().vmovd(dst,mem).unwrap();}}
    else if double {e.u.a.movq(dst,mem).unwrap();}else {e.u.a.movd(dst,mem).unwrap();}
    e.scalar_zero(word&31,double);
}
fn body(u:&mut Unit,pc:u64,p:Plan,r:LoopRa,fast:bool,mask_live:bool) {
    u.region=Some(r);u.rc=RegCache::new();u.vc=VCache::new();u.vzero=[0;2];u.fuse=None;u.tbi_mask_live=mask_live;u.rc_frozen=false;u.dead_flags=false;
    let mut label=r.fast;u.a.set_label(&mut label).unwrap();
    #[cfg(test)] u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),if fast {1i32}else {2i32}).unwrap();
    if super::super::loops::census_enabled() {u.a.mov(rax,pc).unwrap();u.a.mov(qword_ptr(r15+off::PC),rax).unwrap();}
    let index=LOOPREGS[r.slot(p.index).unwrap()];let base=LOOPREGS[r.slot(p.base).unwrap()];let ptr=LOOPREGS[r.slot(p.ptr).unwrap()];
    let mut e=E{u,pc,fuse_in:None,scalar_inputs:None};
    if fast {
        if p.double {direct_load(&mut e,p.words[0],true,qword_ptr(base+index*(1i32<<p.scale)));direct_load(&mut e,p.words[1],true,qword_ptr(ptr+p.offset as i32));}
        else {direct_load(&mut e,p.words[0],false,dword_ptr(base+index*(1i32<<p.scale)));direct_load(&mut e,p.words[1],false,dword_ptr(ptr+p.offset as i32));}
    }else {assert!(e.ldst(p.words[0]));e.pc=pc+4;assert!(e.ldst(p.words[1]));}
    let old_look=std::mem::replace(&mut e.u.look,vec![p.words[3]]);let count=e.u.insns_translated;
    e.pc=pc+8;assert!(e.vchain_pair(p.words[2]));e.u.look=old_look;e.u.insns_translated=count;e.u.skip_next=false;e.u.skip_more=0;
    e.u.a.add(index,p.step as i32).unwrap();e.stx(p.index,index,false);e.u.a.add(ptr,p.stride as i32).unwrap();e.stx(p.ptr,ptr,false);
    e.u.a.cmp(index,p.bound as i32).unwrap();e.u.loop_exit_flags=Some(FuseKind::Sub);e.u.loop_exit_compare=None;e.u.dead_flags=false;e.flags_branch(pc,pc+32,Cc::E);
}
pub(super) fn guards(u:&mut Unit,p:Plan,r:LoopRa,slow:CodeLabel) {
    let index=LOOPREGS[r.slot(p.index).unwrap()];let base=LOOPREGS[r.slot(p.base).unwrap()];let ptr=LOOPREGS[r.slot(p.ptr).unwrap()];
    u.a.cmp(index,(p.bound-p.step)as i32).unwrap();u.a.ja(slow).unwrap();
    if p.step!=1 {u.a.test(index,(p.step-1)as i32).unwrap();u.a.jnz(slow).unwrap();}
    u.a.mov(rax,MASK-p.span_a).unwrap();u.a.cmp(base,rax).unwrap();u.a.ja(slow).unwrap();
    u.a.mov(ecx,p.bound).unwrap();u.a.sub(rcx,index).unwrap();if p.step!=1 {u.a.shr(rcx,p.step.trailing_zeros()).unwrap();}
    u.a.imul_3(rcx,rcx,p.stride as i32).unwrap();u.a.add(rcx,(p.offset+(if p.double {8}else {4})-1)as i32).unwrap();
    u.a.mov(rdx,MASK).unwrap();u.a.sub(rdx,rcx).unwrap();u.a.cmp(ptr,rdx).unwrap();u.a.ja(slow).unwrap();
}
pub(super) fn emit(u:&mut Unit,pc:u64,p:Plan,r:LoopRa) {
    let start=u.a.instructions().len();let mut bad=u.a.create_label();let slow=u.a.create_label();let mask_live=u.tbi_mask_live;
    // Public-only mode guard; read_only_mode and actual output checks enforce
    // stability of this thread-owned Cpu until a canonical exit.
    assert!(GUEST_FAULTS_FATAL);
    u.a.test(dword_ptr(r15+off::FPCR),FPCR_NONDEFAULT as i32).unwrap();u.a.jnz(bad).unwrap();
    guards(u,p,r,slow);
    body(u,pc,p,r,true,mask_live);
    body(u,pc,p,LoopRa{fast:slow,..r},false,mask_live);
    u.cold_paths.push(Box::new(move|u| {u.a.set_label(&mut bad).unwrap();fp_flags::reject(u,pc,r,None);}));
    u.emit_cold_paths();fp_flags::check_mode_emission(u,start).expect("foldaddr native backend contract");
    u.insns_translated+=8;u.foldaddr_emissions+=1;
    if std::env::var_os("ATTESOR_FOLDADDR_LOG").is_some() {eprintln!("foldaddr-loop\t{pc:x}");}
}
#[cfg(test)]mod tests {
    use super::*;
    #[test]fn fpflags_foldaddr_proof_boundaries() {
        let good=[0xfc6d6a61,0xfd4001c2,0x1e620821,0x1e612800,0x910021ad,0x912581ce,0xf12581bf,0x54ffff21];
        let mut u=Unit::new(StubKind::Plain,false);u.foldaddr=true;
        let mut r=LoopRa{pc:0,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};r.reg[..3].copy_from_slice(&[13,14,19]);r.vreg[..3].copy_from_slice(&[0,1,2]);
        let check=|u:&Unit,r:LoopRa,w:&[u32]| {let read=|p|w.get((p/4)as usize).copied();fp_flags::plan(0,&read,&|_|false).and_then(|fp|plan(u,0,fp,r,&read))};
        let p=check(&u,r,&good).unwrap();assert_eq!((p.step,p.stride,p.bound,p.span_a),(8,2400,2400,2399));
        let mut three=good;three[6]=0xf10061bf;assert_eq!(check(&u,r,&three).unwrap().bound,24);
        for(i,bad)in [(0,0xfc6d4a61),(1,0xfd0001c2),(2,0x1e610821),(3,0x1e612801),(4,0x910001ad),(4,0x110021ad),(5,0xd51b4400),(6,0xf12585bf),(7,0x54ffff20)] {let mut w=good;w[i]=bad;assert!(check(&u,r,&w).is_none(),"at={i} word={bad:x}");}
        let read=|p|good.get((p/4)as usize).copied();assert!(fp_flags::plan(0,&read,&|p|p==4).is_none());
        let fp=fp_flags::plan(0,&read,&|_|false).unwrap();assert!(plan(&u,0,fp_flags::Plan{imm:2408,..fp},r,&read).is_none());
        let mut moved=good;moved[7]=0x54ffff41;assert!(plan(&u,0,fp,r,&|p|moved.get((p/4)as usize).copied()).is_none());
        u.foldaddr=false;assert!(check(&u,r,&good).is_none());
    }
}
