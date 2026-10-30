//! Conservative, process-local opt-ins. No environment mutation or app-name heuristics.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Promotion requires independent L2 review. Keep this EMPTY in production by default.
// tools/autotune.py reads this declaration; policy files cannot enlarge it.
pub const L2_ALLOWLIST: &[&str] = &[];
const OPT_INS: &[&str] = &["callret", "regioncache"];
const MAX_BYTES: usize = 65536;
static ACTIVE: OnceLock<Vec<String>> = OnceLock::new();

pub fn enabled(token: &str) -> bool {
    ACTIVE.get().is_some_and(|tokens| tokens.iter().any(|t| t == token))
}
pub fn tokens() -> &'static [String] { ACTIVE.get().map(Vec::as_slice).unwrap_or(&[]) }

fn bundle_id(exe: &Path) -> Option<String> {
    // Canonicalize first: a symlink in another app must not confer its identity.
    let exe = exe.canonicalize().ok()?;
    let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let result = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(bundle.join("Contents/Info.plist")).output().ok()?;
    if !result.status.success() { return None; }
    let id = String::from_utf8(result.stdout).ok()?.trim().to_string();
    valid_id(&id).then_some(id)
}
fn valid_id(id: &str) -> bool {
    id.len() <= 255 && id.contains('.') && id.split('.').all(|s| !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

/// Strict parser for exactly {bundle-id: [token, ...]}. No generic JSON dependency.
/// Reject duplicates, unknown tokens, trailing input and excessive input atomically.
fn parse(data: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    struct Json<'a>(&'a [u8], usize);
    impl Json<'_> {
        fn ws(&mut self) { while self.0.get(self.1).is_some_and(|b| b" \r\n\t".contains(b)) { self.1 += 1; } }
        fn eat(&mut self, b: u8) -> bool { self.ws(); if self.0.get(self.1) == Some(&b) { self.1 += 1; true } else { false } }
        fn need(&mut self, b: u8) -> Result<(), String> { if self.eat(b) { Ok(()) } else { Err("invalid policy JSON".into()) } }
        fn string(&mut self) -> Result<String, String> {
            self.need(b'"')?;
            let mut out = String::new();
            loop {
                let b = *self.0.get(self.1).ok_or("unterminated string")?; self.1 += 1;
                match b {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let b = *self.0.get(self.1).ok_or("bad escape")?; self.1 += 1;
                        out.push(match b {
                            b'"' => '"', b'\\' => '\\', b'/' => '/', b'b' => '\x08', b'f' => '\x0c',
                            b'n' => '\n', b'r' => '\r', b't' => '\t',
                            b'u' => {
                                let bytes = self.0.get(self.1..self.1 + 4).ok_or("bad unicode escape")?;
                                let hex = std::str::from_utf8(bytes).map_err(|_| "bad unicode escape")?;
                                let n = u32::from_str_radix(hex, 16).map_err(|_| "bad unicode escape")?;
                                self.1 += 4;
                                char::from_u32(n).filter(|c| c.is_ascii()).ok_or("policy identifiers must be ASCII")?
                            }
                            _ => return Err("bad escape".into()),
                        });
                    }
                    32..=126 => out.push(b as char),
                    _ => return Err("policy identifiers must be ASCII".into()),
                }
            }
        }
    }
    if data.len() > MAX_BYTES { return Err("policy exceeds 64 KiB".into()); }
    let mut j = Json(data.as_bytes(), 0);
    let mut apps = BTreeMap::new();
    j.need(b'{')?;
    if !j.eat(b'}') {
        loop {
            let id = j.string()?;
            if !valid_id(&id) { return Err("invalid bundle identifier".into()); }
            j.need(b':')?; j.need(b'[')?;
            let mut tokens = Vec::new();
            if !j.eat(b']') {
                loop {
                    let token = j.string()?;
                    if !OPT_INS.contains(&token.as_str()) || tokens.contains(&token) {
                        return Err("unknown or duplicate opt-in token".into());
                    }
                    tokens.push(token);
                    if j.eat(b']') { break; } j.need(b',')?;
                }
            }
            if apps.insert(id, tokens).is_some() { return Err("duplicate bundle identifier".into()); }
            if j.eat(b'}') { break; } j.need(b',')?;
        }
    }
    j.ws();
    if j.1 != j.0.len() { return Err("trailing policy data".into()); }
    Ok(apps)
}

fn select(data: &str, id: &str, allowlist: &[&str], off: &str) -> Result<Vec<String>, String> {
    let apps = parse(data)?;
    let disabled: BTreeSet<_> = off.split(',').collect();
    Ok(apps.get(id).into_iter().flatten()
        .filter(|t| allowlist.contains(&t.as_str()) && !disabled.contains(t.as_str()))
        .cloned().collect())
}

/// Call once before any code-generation configuration or guest threads initialize.
/// Prewarm has no single app identity and deliberately uses no automatic opt-ins.
pub fn initialize(exe: &Path) {
    ACTIVE.get_or_init(|| {
        if std::env::var("ATTESOR_AUTOTUNE").as_deref() == Ok("0") { return Vec::new(); }
        let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
        let path = PathBuf::from(home).join("Library/Application Support/Attesor/policy.json");
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => { eprintln!("attesor: policy ignored: {e}"); return Vec::new(); }
        };
        let result = (|| {
            let mut data = String::new();
            (&mut file).take((MAX_BYTES + 1) as u64).read_to_string(&mut data).map_err(|e| e.to_string())?;
            // Validate even when no app identity is available. A bad unrelated row
            // invalidates the complete file; nothing is partially applied.
            parse(&data)?;
            let Some(id) = bundle_id(exe) else { return Ok(Vec::new()) };
            select(&data, &id, L2_ALLOWLIST, &std::env::var("ATTESOR_JIT_OFF").unwrap_or_default())
        })();
        result.unwrap_or_else(|e: String| { eprintln!("attesor: policy ignored: {e}"); Vec::new() })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation() {
        for data in ["", "[]", "null", "{}{}", r#"{"com.x":true}"#, r#"{"com.x":[1]}"#,
            r#"{"com.x":["callret",]}"#, r#"{"com.x":[],}"#, r#"{"com.x":[],"com.x":[]}"#,
            r#"{"com.x":["callret","callret"]}"#, r#"{"com.x":["loopra"]}"#,
            r#"{"*":[]}"#, r#"{"com..x":[]}"#, r#"{"com.x":["\uD800"]}"#] {
            assert!(parse(data).is_err(), "accepted {data}");
        }
        assert!(parse(&" ".repeat(MAX_BYTES + 1)).is_err());
        assert!(parse("{}\n").unwrap().is_empty());
        assert_eq!(parse(r#"{"com.x":["\u0063allret"]}"#).unwrap()["com.x"], ["callret"]);
    }
    #[test]
    fn canonical_nearest_bundle_identity() {
        let dir = std::env::temp_dir().join(format!("attesor-policy-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let bundle = dir.join("Outer.app");
        let macos = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("probe");
        std::fs::write(&exe, "fixture").unwrap();
        let plist = bundle.join("Contents/Info.plist");
        std::fs::write(&plist, r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.Outer</string></dict></plist>"#).unwrap();
        assert_eq!(bundle_id(&exe).as_deref(), Some("com.example.Outer"));
        assert!(std::process::Command::new("/usr/bin/plutil").args(["-convert", "binary1"])
            .arg(&plist).status().unwrap().success());
        assert_eq!(bundle_id(&exe).as_deref(), Some("com.example.Outer"));
        let plain = dir.join("plain");
        std::fs::write(&plain, "fixture").unwrap();
        std::os::unix::fs::symlink(&plain, macos.join("alias")).unwrap();
        assert!(bundle_id(&macos.join("alias")).is_none());
        let inner = bundle.join("Contents/Helpers/Inner.app/Contents/MacOS/probe");
        std::fs::create_dir_all(inner.parent().unwrap()).unwrap();
        std::fs::write(&inner, "fixture").unwrap();
        assert!(bundle_id(&inner).is_none()); // never borrow the parent's ID
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn conservative_selection() {
        let data = r#"{"com.x":["callret","regioncache"],"com.y":[]}"#;
        assert!(select(data, "com.x", L2_ALLOWLIST, "").unwrap().is_empty());
        assert_eq!(select(data, "com.x", &["callret"], "").unwrap(), ["callret"]);
        assert!(select(data, "com.x", &["callret"], "callret").unwrap().is_empty());
        assert!(select(data, "com.y", &["callret"], "").unwrap().is_empty());
        assert!(select(data, "com.z", &["callret"], "").unwrap().is_empty());
        assert!(!enabled("callret"));
    }
}
