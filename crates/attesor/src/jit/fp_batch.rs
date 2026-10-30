//! Four ordered scalar MAC iterations, with immutable raw-load captures and
//! strict cold NaN replay. No data reread, FMA, reassociation or public side entry.
use super::*;
#[derive(Clone,Copy)]
pub(super) struct Plan {
    base:u32,index:u32,ptr:u32,step:u32,stride:u32,bound:u32,scale:u32,offset:u32,
    t:u32,m:u32,d:u32,double:bool,product_first:bool,temps:[AsmRegisterXmm;7],
}
fn add_self(w:u32,n:u32)->Option<u32> {
    (w&0xff800000==0x91000000&&w&31==n&&bits(w,9,5)==n).then(||bits(w,21,10)<<if bit(w,22)!=0 {12}else {0})
}
pub(super) fn plan(u:&Unit,pc:u64,fp:fp_flags::Plan,r:LoopRa,read:&dyn Fn(u64)->Option<u32>)->Option<Plan> {
    pc.checked_add(32)?;
    if !u.vbatch||!u.avx512||!has_avx512()||fp.cmp_pc!=pc.checked_add(24)?||!fp.sf||fp.imm<=0||!fp_flags::read_only_mode(pc,fp,read) {return None;}
    let mut w=[0;8];for(i,v)in w.iter_mut().enumerate(){*v=read(pc.checked_add(i as u64*4)?)?;}
    if w[7]&0xff00001f!=0x54000001||w[0]&0x3b200c00!=0x38200800||w[1]&0x3b000000!=0x39000000 {return None;}
    if [w[0],w[1]].iter().any(|&v|bit(v,26)==0||v>>30<2||bits(v,23,22)!=1)||w[0]>>30!=w[1]>>30||bits(w[0],15,13)!=3 {return None;}
    let double=w[0]>>30==3;let ty=double as u32;let t=w[0]&31;let m=w[1]&31;let d=w[3]&31;
    if t==m||t==d||m==d||w[2]&0xff20fc00!=0x1e200800||w[3]&0xff20fc00!=0x1e202800
        ||bits(w[2],23,22)!=ty||bits(w[3],23,22)!=ty||w[2]&31!=t||bits(w[2],9,5)!=t||bits(w[2],20,16)!=m {return None;}
    let(an,am)=(bits(w[3],9,5),bits(w[3],20,16));if !((an==d&&am==t)||(an==t&&am==d)) {return None;}
    let base=bits(w[0],9,5);let index=bits(w[0],20,16);let ptr=bits(w[1],9,5);
    if [base,index,ptr].contains(&31)||base==index||base==ptr||index==ptr||fp.rn!=index {return None;}
    let step=add_self(w[4],index)?;let stride=add_self(w[5],ptr)?;let bound=fp.imm as u32;
    let chunk=step.checked_mul(4)?;if !step.is_power_of_two()||bound<chunk||bound%chunk!=0 {return None;}
    for n in [base,index,ptr] {r.slot(n)?;}for n in [t,m,d] {if high_xmm(VREGS[r.vslot(n)?]) {return None;}}
    let free:Vec<_>=r.vreg.iter().enumerate().filter(|(_,n)|**n==0xff).map(|(i,_)|VREGS[i]).take(7).collect();
    if free.len()!=7||free[..4].iter().any(|&v|high_xmm(v)) {return None;}
    let temps=free.try_into().ok()?;
    Some(Plan{base,index,ptr,step,stride,bound,scale:if bit(w[0],12)!=0 {w[0]>>30}else {0},offset:bits(w[1],21,10)<<(w[1]>>30),t,m,d,double,product_first:an==t,temps})
}
pub(super) fn enter(u:&mut Unit,p:Plan,r:LoopRa) {
    let mut bad=u.a.create_label();let index=LOOPREGS[r.slot(p.index).unwrap()];let chunk=4*p.step;
    u.a.cmp(index,(p.bound-chunk)as i32).unwrap();u.a.ja(bad).unwrap();u.a.test(index,(chunk-1)as i32).unwrap();u.a.jnz(bad).unwrap();
    u.cold_paths.push(Box::new(move|u|{u.a.set_label(&mut bad).unwrap();fp_flags::reject(u,r.pc,r,None);}));
}
fn source(r:AsmRegisterXmm)->FpSource {FpSource{cached:Some(r),offset:0}}
fn load(e:&mut E<'_>,dst:AsmRegisterXmm,double:bool) {
    if high_xmm(dst) {if double {e.u.a.evex().vmovq(dst,qword_ptr(rax)).unwrap();}else {e.u.a.evex().vmovd(dst,dword_ptr(rax)).unwrap();}}
    else if double {e.u.a.vmovq(dst,qword_ptr(rax)).unwrap();}else {e.u.a.vmovd(dst,dword_ptr(rax)).unwrap();}
}
fn unpack_b(e:&mut E<'_>,p:Plan,packed:AsmRegisterXmm) {
    if p.double {
        e.u.a.vmovhlps(xmm2,xmm2,packed).unwrap();e.u.a.vmovq(xmm2,xmm2).unwrap();
    }else {e.u.a.vpsrldq(xmm2,packed,4).unwrap();}
}
/// Cold replay writes only scratch0..7 and the three reserved cold work locations.
/// Captures/original V stay immutable; all memory reads are Cpu or RIP constants.
fn check_cold(u:&Unit,start:usize,p:Plan) {
    use iced_x86::{InstructionInfoFactory,Register,OpAccess};let mut f=InstructionInfoFactory::new();
    for insn in &u.a.instructions()[start..] {
        let info=f.info(insn);
        for reg in info.used_registers() {
            if !matches!(reg.access(),OpAccess::Write|OpAccess::CondWrite|OpAccess::ReadWrite|OpAccess::ReadCondWrite) {continue;}
            let r=reg.register();let allowed=if r.is_vector_register(){r.number()<8||[p.temps[4],p.temps[5],p.temps[6]].iter().any(|&v|iced_x86::Register::from(v).number()==r.number())}
                else {matches!(r.full_register(),Register::RAX|Register::RCX|Register::RDX|Register::K1)};
            assert!(allowed,"vbatch cold clobber {r:?}");
        }
        for m in info.used_memory() {assert!(matches!(m.base(),Register::R15|Register::RIP|Register::None)&&m.index()==Register::None,"vbatch cold guest-memory reread");}
    }
}
pub(super) fn emit(u:&mut Unit,pc:u64,p:Plan) {
    let hot_start=u.a.instructions().len();
    let r=u.region.unwrap();let index=LOOPREGS[r.slot(p.index).unwrap()];let base=LOOPREGS[r.slot(p.base).unwrap()];let ptr=LOOPREGS[r.slot(p.ptr).unwrap()];
    let old=source(VREGS[r.vslot(p.d).unwrap()]);let mut e=E{u,pc,fuse_in:None,scalar_inputs:None};
    // Every public/private entry to this batch establishes the original mask.
    if !e.u.tbi_mask_live {e.u.a.mov(r11,0x00ff_ffff_ffff_ffffu64).unwrap();e.u.tbi_mask_live=true;}
    for i in 0..4usize {
        e.u.a.lea(rax,qword_ptr(base+index*(1i32<<p.scale)+(i as u32*p.step*(1<<p.scale))as i32)).unwrap();e.u.a.and(rax,r11).unwrap();load(&mut e,xmm2,p.double);
        e.u.a.lea(rax,qword_ptr(ptr+(p.offset+i as u32*p.stride)as i32)).unwrap();e.u.a.and(rax,r11).unwrap();load(&mut e,xmm3,p.double);
        if p.double {e.u.a.vunpcklpd(p.temps[i],xmm2,xmm3).unwrap();}else {e.u.a.vpunpckldq(p.temps[i],xmm2,xmm3).unwrap();}
        e.fp_register(0,p.double,xmm0,xmm2,xmm3);
        e.fp_register(2,p.double,xmm1,xmm0,if i==0 {old.cached.unwrap()}else {xmm1});
    }
    let mut cold=e.u.a.create_label();let mut resume=e.u.a.create_label();
    if p.double {e.u.a.vucomisd(xmm1,xmm1).unwrap();}else {e.u.a.vucomiss(xmm1,xmm1).unwrap();}e.u.a.jp(cold).unwrap();e.u.a.set_label(&mut resume).unwrap();
    e.u.cold_paths.push(Box::new(move|u| {
        u.a.set_label(&mut cold).unwrap();
        #[cfg(test)] u.a.inc(qword_ptr(r15+off::AOT_LOOP_REGIONS)).unwrap();
        let start=u.a.instructions().len();
        let mut e=E{u,pc,fuse_in:None,scalar_inputs:None};
        for i in 0..4 {
            unpack_b(&mut e,p,p.temps[i]);e.cache_copy(p.temps[6],xmm2);
            let a=source(p.temps[i]);let b=source(p.temps[6]);
            e.fld_source(xmm0,a,p.double);e.fld_source(xmm2,b,p.double);e.fp_register(0,p.double,xmm0,xmm0,xmm2);e.scalar_nan(p.double,a,Some(b));e.cache_copy(p.temps[4],xmm0);
            let acc=if i==0 {old}else {source(p.temps[5])};let product=source(p.temps[4]);
            let(n,m)=if p.product_first {(product,acc)}else {(acc,product)};
            e.fld_source(xmm0,n,p.double);e.fld_source(xmm2,m,p.double);e.fp_register(2,p.double,xmm0,xmm0,xmm2);e.scalar_nan(p.double,n,Some(m));e.cache_copy(p.temps[5],xmm0);
        }
        e.cache_copy(xmm0,p.temps[4]);e.cache_copy(xmm1,p.temps[5]);
        check_cold(e.u,start,p);e.u.a.jmp(resume).unwrap();
    }));
    unpack_b(&mut e,p,p.temps[3]);
    for (guest,src) in [(p.t,xmm0),(p.d,xmm1),(p.m,xmm2)] {
        let dst=VREGS[r.vslot(guest).unwrap()];
        if high_xmm(dst)||high_xmm(src) {e.u.a.vmovdqa64(dst,src).unwrap();}else {e.u.a.vmovdqa(dst,src).unwrap();}
        e.scalar_zero(guest,p.double);
    }
    e.u.a.add(index,(4*p.step)as i32).unwrap();e.stx(p.index,index,false);e.u.a.add(ptr,(4*p.stride)as i32).unwrap();e.stx(p.ptr,ptr,false);
    e.u.a.cmp(index,p.bound as i32).unwrap();e.u.loop_exit_flags=Some(FuseKind::Sub);e.u.loop_exit_compare=None;e.u.dead_flags=false;
    e.flags_branch(pc,pc+32,Cc::E);
    assert!(e.u.a.instructions()[hot_start..].iter().all(|i|i.code().encoding()!=iced_x86::EncodingKind::EVEX),"vbatch hot high-bank instruction");
    e.u.insns_translated+=8;e.u.vbatch_emissions+=1;
    if std::env::var_os("ATTESOR_VBATCH_LOG").is_some() {eprintln!("vbatch-loop\t{pc:x}");}
}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn fpflags_vbatch_proof_boundaries_and_tagged_arithmetic() {
        if !has_avx512(){return;}
        let good=[0xfc6d6a61,0xfd4001c2,0x1e620821,0x1e612800,0x910021ad,0x912581ce,0xf12581bf,0x54ffff21];
        let mut u=Unit::new(StubKind::Plain,false);u.vbatch=true;u.avx512=true;
        let mut r=LoopRa{pc:0,fast:u.a.create_label(),exit:u.a.create_label(),reg:[0xff;NLOOPREGS],dirty:0,vreg:[0xff;NVREGS],xreg:[0xff;NVREGS],flags:0xff};r.reg[..3].copy_from_slice(&[13,14,19]);r.vreg[..3].copy_from_slice(&[0,1,2]);
        let check=|u:&Unit,r:LoopRa,w:&[u32]| {let read=|p|w.get((p/4)as usize).copied();fp_flags::plan(0,&read,&|_|false).and_then(|fp|plan(u,0,fp,r,&read))};
        let near=u64::MAX-31;let fp=fp_flags::Plan{cmp_pc:near+24,sf:true,rn:13,imm:2400};
        assert!(plan(&u,near,fp,r,&|at|good.get((at.wrapping_sub(near)/4)as usize).copied()).is_none());
        let p=check(&u,r,&good).unwrap();assert_eq!((p.step,p.stride,p.bound),(8,2400,2400));
        for(i,bad)in [(0,0xfc6d4a61),(1,0xfd0001c2),(2,0x1e610821),(3,0x1e612801),(4,0x910001ad),(4,0x110021ad),(5,0xd51b4400),(6,0xf12585bf),(7,0x54ffff20)] {let mut w=good;w[i]=bad;assert!(check(&u,r,&w).is_none(),"at={i} word={bad:x}");}
        let mut crowded=r;for i in 0..15 {crowded.vreg[i]=i as u8;}assert!(check(&u,crowded,&good).is_none());u.avx512=false;assert!(check(&u,r,&good).is_none());
        for raw in [0u64,0xab00000000000000,u64::MAX-3,u64::MAX-3000] {let mut pointer=raw;for i in 0..4u64 {assert_eq!(pointer&0x00ffffffffffffff,raw.wrapping_add(i*2400)&0x00ffffffffffffff);pointer=pointer.wrapping_add(2400);}assert_eq!(pointer,raw.wrapping_add(9600));}
    }
}
