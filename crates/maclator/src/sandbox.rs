//! App Sandbox containers for sandboxed guests.
//!
//! libsystem_secinit decides whether to enter the sandbox from the entitlements of the running
//! process, which is maclator itself, so sandboxed arm64 apps used to run uncontained: their data
//! landed in ~/Documents etc. instead of the container they use natively. When the guest's code
//! signature carries com.apple.security.app-sandbox, point HOME/CFFIXED_USER_HOME/TMPDIR at the
//! same container (~/Library/Containers/<identifier>/Data) the app gets on Apple Silicon.
//! The kernel sandbox profile itself is not applied (the host process must keep its own access).

use crate::macho::MachO;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const LC_CODE_SIGNATURE: u32 = 0x1d;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSMAGIC_EMBEDDED_ENTITLEMENTS: u32 = 0xfade_7171;

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// (signing identifier, entitlements XML) from the arm64 slice's embedded code signature.
fn signature(exe: &str) -> Option<(Option<String>, Option<String>)> {
    let m = MachO::open_arm64(exe, true).ok()?;
    let all = &m.header_bytes;
    let ncmds = u32::from_le_bytes(all[16..20].try_into().ok()?);
    let mut p = 32usize;
    let mut cs = None;
    for _ in 0..ncmds {
        let cmd = u32::from_le_bytes(all.get(p..p + 4)?.try_into().ok()?);
        let size = u32::from_le_bytes(all.get(p + 4..p + 8)?.try_into().ok()?) as usize;
        if cmd == LC_CODE_SIGNATURE {
            let off = u32::from_le_bytes(all.get(p + 8..p + 12)?.try_into().ok()?) as u64;
            let len = u32::from_le_bytes(all.get(p + 12..p + 16)?.try_into().ok()?) as usize;
            cs = Some((off, len));
        }
        p += size;
    }
    let (off, len) = cs?;
    let mut f = File::open(exe).ok()?;
    f.seek(SeekFrom::Start(m.slice_offset + off)).ok()?;
    let mut sb = vec![0u8; len.min(16 << 20)];
    f.read_exact(&mut sb).ok()?;
    if be32(&sb, 0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        return None;
    }
    let (mut ident, mut ents) = (None, None);
    for i in 0..be32(&sb, 8)? as usize {
        let bo = be32(&sb, 12 + i * 8 + 4)? as usize;
        let blen = be32(&sb, bo + 4)? as usize;
        let blob = sb.get(bo..bo + blen)?;
        match be32(blob, 0)? {
            CSMAGIC_CODEDIRECTORY if ident.is_none() => {
                let io = be32(blob, 20)? as usize;
                let s = blob.get(io..)?;
                let end = s.iter().position(|&c| c == 0)?;
                ident = Some(String::from_utf8_lossy(&s[..end]).into_owned());
            }
            CSMAGIC_EMBEDDED_ENTITLEMENTS => ents = Some(String::from_utf8_lossy(&blob[8..]).into_owned()),
            _ => {}
        }
    }
    Some((ident, ents))
}

fn plist_true(xml: &str, key: &str) -> bool {
    let k = format!("<key>{key}</key>");
    xml.find(&k).map(|i| xml[i + k.len()..].trim_start().starts_with("<true/>")).unwrap_or(false)
}

/// CFBundleIdentifier of the .app containing `exe` (…/X.app/Contents/MacOS/exe), if any.
fn bundle_id(exe: &str) -> Option<String> {
    let macos = std::path::Path::new(exe).parent()?;
    let info = std::fs::read_to_string(macos.parent()?.join("Info.plist")).ok()?;
    let k = "<key>CFBundleIdentifier</key>";
    let rest = info[info.find(k)? + k.len()..].trim_start().strip_prefix("<string>")?;
    Some(rest[..rest.find("</string>")?].trim().to_string())
}

fn set(envp: &mut Vec<String>, key: &str, val: &str) {
    envp.retain(|e| !e.starts_with(&format!("{key}=")));
    envp.push(format!("{key}={val}"));
}

/// Redirect a sandboxed guest into its container. Returns the container path when applied.
pub fn apply(exe: &str, envp: &mut Vec<String>) -> Option<String> {
    if std::env::var_os("MACLATOR_NO_CONTAINER").is_some() {
        return None;
    }
    let (ident, ents) = signature(exe)?;
    if !plist_true(&ents?, "com.apple.security.app-sandbox") {
        return None;
    }
    // Already inside a container (child of a sandboxed guest, or a nested helper): keep it.
    if envp.iter().any(|e| e.starts_with("APP_SANDBOX_CONTAINER_ID=")) {
        return None;
    }
    let id = bundle_id(exe).or(ident)?;
    let home = std::env::var("HOME").ok()?;
    let data = format!("{home}/Library/Containers/{id}/Data");
    for d in ["Documents", "Library/Application Support", "Library/Caches", "Library/Preferences", "tmp"] {
        let _ = std::fs::create_dir_all(format!("{data}/{d}"));
    }
    set(envp, "HOME", &data);
    set(envp, "CFFIXED_USER_HOME", &data);
    set(envp, "TMPDIR", &format!("{data}/tmp/"));
    set(envp, "APP_SANDBOX_CONTAINER_ID", &id);
    Some(data)
}
