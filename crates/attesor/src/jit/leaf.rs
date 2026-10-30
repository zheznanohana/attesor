//! Exact, bounded live-ins for read-only straight-line GPR return blocks.
//! No function/algorithm identity, guessed access census or recursive marker.
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct Plan {
    pub words:BTreeMap<u64,u32>,
    pub livein:u32,
    pub used:u32,
    pub written:u32,
}
impl Plan {
    /// Only a final CMP followed solely by native flag-preserving selects can
    /// defer its Cpu NZCV stores. Never infer this from guest ABI volatility.
    pub(super) fn final_fused_cmp(&self)->Option<u64> {
        let ret=*self.words.last_key_value()?.0;
        let (&pc,_)=self.words.iter().rev().find(|(_,w)|matches!(super::fusable_setter(**w),Some(super::FuseKind::Sub)))?;
        self.words.range((std::ops::Bound::Excluded(pc),std::ops::Bound::Excluded(ret)))
            .all(|(_,w)|super::fused_select(super::FuseKind::Sub,*w)).then_some(pc)
    }
}

fn gpr(n:u32,sp:bool)->u32 {if n==31&&!sp {0}else {1u32<<n}}
fn access(w:u32)->Option<(u32,u32)> {
    let (d,n,m)=(w&31,(w>>5)&31,(w>>16)&31);
    let write=gpr(d,false);
    if w==0xd503201f {return Some((0,0));}
    if w==0xd65f03c0 {return Some((1<<30,0));}
    // Ordinary unsigned-offset W/X loads only: no sign extension, SIMD,
    // pre/post writeback, acquire/atomic, helpers or stores.
    if w&0x3fc00000==0x39400000 && w>>30>=2 {
        return Some((gpr(n,true),write));
    }
    // CMP shifted register, LSL #0 (XZR); CMP immediate (SP).
    if w&0x7fe0fc1f==0x6b00001f {return Some((gpr(n,false)|gpr(m,false),0));}
    if w&0x7f80001f==0x7100001f {return Some((gpr(n,true),0));}
    // All ordinary CSEL/CSINC/CSINV/CSNEG variants, both widths. Both
    // alternatives are live even for an always condition: conservative only.
    if w&0x3fe00800==0x1a800000 {return Some((gpr(n,false)|gpr(m,false),write));}
    // MOV register alias with no shift/inversion; W writes define full X.
    if w&0x7fe0ffe0==0x2a0003e0 {return Some((gpr(m,false),write));}
    if w&0x1f800000==0x12800000 {
        let opc=(w>>29)&3;
        if opc==1 || (w>>31==0&&(w>>21)&3>=2) {return None;}
        return Some((if opc==3 {write}else {0},write));
    }
    None
}

pub(super) fn plan(pc:u64,read:&dyn Fn(u64)->Option<u32>,cut:&dyn Fn(u64)->bool)->Option<Plan> {
    let mut p=pc;let mut words=BTreeMap::new();let mut livein=0;let mut defined=0;let mut used=0;let mut accesses=0;
    for _ in 0..32 {
        if p!=pc&&cut(p) {return None;}
        let w=read(p)?;let(r,d)=access(w)?;
        livein|=r&!defined;defined|=d;used|=r|d;accesses+=(r|d).count_ones();words.insert(p,w);
        if w==0xd65f03c0 {
            // A profitability bound, not a correctness decoder for unknowns.
            return (accesses>=6 && defined.count_ones()>=2 && used.count_ones()<=super::NLOOPREGS as u32)
                .then_some(Plan{words,livein,used,written:defined});
        }
        p=p.checked_add(4)?;
    }
    None
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn leafflags_final_cmp_needs_honest_fused_suffix() {
        let words=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
        let p=plan(0,&|p|words.get((p/4)as usize).copied(),&|_|false).unwrap();assert_eq!(p.final_fused_cmp(),Some(8));
        for extra in [0xd503201f,0xd2800011,0xb940004f] {let mut w=words.to_vec();w.insert(3,extra);
            assert_eq!(plan(0,&|p|w.get((p/4)as usize).copied(),&|_|false).unwrap().final_fused_cmp(),None);}
        let mut w=words;w[3]=0x1a9fe7e8;assert_eq!(plan(0,&|p|w.get((p/4)as usize).copied(),&|_|false).unwrap().final_fused_cmp(),None);
    }
    #[test] fn leafra_livein_exact_access_and_boundaries() {
        let words=[0xb9400008,0xb9400029,0x6b09011f,0x1a9fd7e8,0x5a9fa100,0xd65f03c0];
        let read=|p|words.get((p/4)as usize).copied();let p=plan(0,&read,&|_|false).unwrap();
        assert_eq!(p.livein,(1<<0)|(1<<1)|(1<<30));assert_eq!(p.used,p.livein|(1<<8)|(1<<9));
        assert!(plan(0,&read,&|p|p==8).is_none());
        for w in [0xb9000008,0xf8408408,0xbd400008,0x99400008,0xd5033bbf,0xd53b4200,0x94000000,0x14000000,0x54000000,0xd65f0000,0x12c00000,0x32800000] {assert!(access(w).is_none(),"{w:08x}");}
        assert_eq!(access(0xf94003e8),Some((1<<31,1<<8)));
        assert_eq!(access(0xaa1f03e8),Some((0,1<<8)));
        assert_eq!(access(0xf2800028),Some((1<<8,1<<8)));
        assert_eq!(access(0x52800028),Some((0,1<<8)));
        assert!(plan(0,&|p|if p==0 {Some(words[0])}else {None},&|_|false).is_none());
        assert!(plan(0,&|_|Some(0xd503201f),&|_|false).is_none());
    }
}
