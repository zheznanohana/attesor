//! NZCV sinking in one scalar-FP loop, with exact repair at its FPCR guard.
//! Public entry retains incoming NZCV; a private backedge has the known tail CMP.
use super::*;

#[derive(Clone,Copy,Debug)]
pub(super) struct Plan {pub cmp_pc:u64,pub sf:bool,pub rn:u32,pub imm:i32}

fn memory(w:u32)->bool {
    let single=w&0x3b000000==0x39000000||w&0x3b200000==0x38000000||w&0x3b200c00==0x38200800;
    if w&(1<<26)!=0 {
        return single&&w>>30>=2&&(w>>22)&3<=1&&flags_safe_passthrough(w);
    }
    if w&0x3a000000==0x28000000 {
        let load=w&(1<<22)!=0;let size=w>>30;
        return size!=3&&(size!=1||load)&&(!load||(w&31)!=((w>>10)&31));
    }
    single&&flags_safe_passthrough(w)&&!((w>>22)&3==3&&w>>30>=2)
}

pub(super) fn plan(pc:u64,read:&dyn Fn(u64)->Option<u32>,cut:&dyn Fn(u64)->bool)->Option<Plan> {
    let mut previous:Option<(u64,u32)>=None;let mut fp=false;
    for i in 0..MAX_BLOCK_INSNS {
        let at=pc.checked_add(i as u64*4)?;
        if i!=0&&cut(at) {return None;}
        let w=read(at)?;
        if is_terminator(w) {
            if !fp||w&0xff000010!=0x54000000||w&15>=14 {return None;}
            let target=at.wrapping_add((sext(((w>>5)&0x7ffff)as u64,19)<<2)as u64);
            let (cmp_pc,cmp)=previous?;
            // Exact CMP Wn/Xn,#imm{,LSL#12}, not CMN, SP or reserved.
            if target!=pc||cmp&0x7f80001f!=0x7100001f||(cmp>>5)&31==31 {return None;}
            return Some(Plan{cmp_pc,sf:cmp>>31!=0,rn:(cmp>>5)&31,
                imm:(((cmp>>10)&0xfff)<<if cmp&(1<<22)!=0 {12}else {0})as i32});
        }
        let scalar=(w&0xff200c00==0x1e200800&&(w>>12)&15<=3
            ||w&0xff3ffc00==0x1e21c000)&&bits(w,23,22)<=1;
        if scalar {fp=true;}
        else if (w>>25)&5==4 {if !memory(w) {return None;}}
        else if reads_flags(w)||(!flags_safe_passthrough(w)&&!flags_safe_writer(w)) {return None;}
        previous=Some((at,w));
    }
    None
}

/// Outlined exit: no fallthrough/resume and no compiler cache inference from
/// the body. The captured fixed assignment is the exact private entry contract;
/// the region's existing dispatcher tail performs its canonical writeback.
pub(super) fn reject(u:&mut Unit,pc:u64,region:LoopRa,cmp:Option<Plan>) {
    let saved_region=u.region.replace(region);
    let saved_dead=std::mem::replace(&mut u.dead_flags,false);
    let saved_rc=std::mem::replace(&mut u.rc,RegCache::new());
    let mut e=E{u,pc,fuse_in:None,scalar_inputs:None};
    if let Some(p)=cmp {e.fpflags_compare(p);e.flags_sub();}
    e.mov_imm(rax,pc);
    e.u.a.mov(qword_ptr(r15+off::PC),rax).unwrap();
    e.u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_INTERP as i32).unwrap();
    e.u.a.jmp(region.exit).unwrap();
    e.u.region=saved_region;e.u.dead_flags=saved_dead;e.u.rc=saved_rc;
}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn fpflags_planner_native_observer_boundaries() {
        let good=[0xfd400001,0x1e610821,0x1e612800,0x9100054a,0xf1000d5f,0x54ffff61];
        let get=|p|good.get((p/4)as usize).copied();let p=plan(0,&get,&|_|false).unwrap();assert_eq!((p.cmp_pc,p.sf,p.rn,p.imm),(16,true,10,3));
        assert!(plan(0,&get,&|p|p==4).is_none());
        for bad in [0x9a890108,0xd53b4200,0xd4001001,0x94000000,0x1e602000,0x4e61d400,0xbdc00001,0x69000000,0x29400000,0xb9c00000] {
            let mut w=good;w[1]=bad;assert!(plan(0,&|p|w.get((p/4)as usize).copied(),&|_|false).is_none(),"{bad:08x}");
        }
        for bad in [0xb5ffff6a,0x54ffff60|(14),0xf1000fff] {
            let mut w=good;if bad&0xff000000==0xf1000000 {w[4]=bad;}else {w[5]=bad;}
            assert!(plan(0,&|p|w.get((p/4)as usize).copied(),&|_|false).is_none());
        }
    }
}

/// A Cpu is owned by one engine thread (threads.rs/engine.rs); GDB runs that
/// thread in the interpreter, not concurrently. Async guest-handler delivery is
/// absent. For this closed read-only native subset, neither guest stores/system
/// operations nor helpers can change its FPCR between private iterations.
/// Keep this proof separate from ordinary flags-deadness, which admits stores.
pub(super) fn read_only_mode(pc:u64,p:Plan,read:&dyn Fn(u64)->Option<u32>)->bool {
    if !GUEST_FAULTS_FATAL {return false;}
    let mut at=pc;
    while at<p.cmp_pc {
        let Some(w)=read(at) else {return false;};
        let scalar=(w&0xff200c00==0x1e200800&&(w>>12)&15<=3||w&0xff3ffc00==0x1e21c000)&&bits(w,23,22)<=1;
        let load=w&(1<<26)!=0&&w>>30>=2&&bits(w,23,22)==1&&flags_safe_passthrough(w)
            &&(w&0x3b000000==0x39000000||w&0x3b200c00==0x38200800||w&0x3b200c00==0x38000000);
        let gp=w&0x3f800000==0x11000000; // ADD/SUB immediate !S, valid W/X
        if !scalar&&!load&&!gp {return false;}
        let Some(next)=at.checked_add(4) else {return false;};at=next;
    }
    true
}

/// Check the actual complete hot+cold output too. An emitter regression must
/// fail closed before any code is registered/executed, not silently invalidate
/// the static read-only/mode-stability contract. Only disjoint, known r15 Cpu
/// writes are permitted: no CALL, guest-memory store or FPCR-byte overlap.
pub(super) fn check_mode_emission(u:&Unit,start:usize)->Result<(),&'static str> {
    use iced_x86::{InstructionInfoFactory,OpAccess,Register,Mnemonic};
    let mut info=InstructionInfoFactory::new();
    for insn in &u.a.instructions()[start..] {
        if matches!(insn.mnemonic(),Mnemonic::Call|Mnemonic::Ldmxcsr|Mnemonic::Vldmxcsr) {return Err("native helper/control observer");}
        for m in info.info(insn).used_memory() {
            if !matches!(m.access(),OpAccess::Write|OpAccess::CondWrite|OpAccess::ReadWrite|OpAccess::ReadCondWrite) {continue;}
            let len=m.memory_size().size() as u64;let begin=m.displacement();
            if !(m.base()==Register::R15&&m.index()==Register::None&&len!=0
                &&begin.checked_add(len).is_some_and(|end|end<=off::FPCR as u64||begin>=off::FPCR as u64+8)) {return Err("unexpected memory/FPCR write");}
        }
    }
    Ok(())
}

#[cfg(test)]mod mode_tests {
    use super::*;
    #[test]fn fpflags_mode_proof_and_emitted_observer_barriers() {
        let good=[0xfd400001,0x1e610821,0x1e612800,0x9100054a,0xf1000d5f,0x54ffff61];
        let check=|w:&[u32]| {let read=|p|w.get((p/4)as usize).copied();plan(0,&read,&|_|false).is_some_and(|p|read_only_mode(0,p,&read))};
        assert!(check(&good));let mut unscaled=good;unscaled[0]=0xfc400001;assert!(check(&unscaled));
        for w in [0xfd000001,0xb9000043,0xfc008401,0xfc408401,0xf940000a,0xd53b4400,0xd51b4400,0x94000000,0xd4001001,0x1e602000,0x1e624000,0x1ac00c01] {
            let mut bad=good;bad[0]=w;assert!(!check(&bad),"{w:x}");
        }
        for bad in 0..4 {
            let mut u=Unit::new(StubKind::Plain,false);
            match bad {0=>u.a.call(rax).unwrap(),1=>u.a.mov(qword_ptr(r15+off::FPCR),0i32).unwrap(),
                2=>u.a.mov(byte_ptr(rax),1i32).unwrap(),_=>u.a.mov(qword_ptr(r15+off::FPCR-4),0i32).unwrap()}
            assert!(check_mode_emission(&u,0).is_err());
        }
        let mut u=Unit::new(StubKind::Plain,false);u.a.mov(qword_ptr(r15+off::FPCR-8),0i32).unwrap();assert!(check_mode_emission(&u,0).is_ok());
    }
}
