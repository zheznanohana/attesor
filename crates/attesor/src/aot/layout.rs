//! Explicit image-relative sample weights for offline placement, not execution counters.
//!
//! The ordinary AOT profile is a set of PCs; it cannot supply a frequency. Only
//! an explicitly supplied, bounded weight file authorizes frequency ordering.
use super::Image;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Error, ErrorKind};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_IMAGES: usize = 256;
const HEADER: &str = "Attesor AOT layout samples v1";

struct ImageSamples {
    size: u64,
    weights: BTreeMap<u64, u64>,
}
struct Profile {
    identity: u64,
    images: BTreeMap<String, ImageSamples>,
}
static PROFILE: OnceLock<Option<Profile>> = OnceLock::new();

fn enabled() -> bool {
    let has = |name: &str, token: &str| std::env::var(name).unwrap_or_default()
        .split(',').any(|s| s == token);
    has("ATTESOR_JIT_ON", "offline-layout-profile")
        && !has("ATTESOR_JIT_OFF", "offline-layout")
        && !has("ATTESOR_JIT_OFF", "offline-layout-profile")
}
fn invalid() -> Error { Error::new(ErrorKind::InvalidData, "invalid AOT layout sample profile") }
fn number(s: &str) -> std::io::Result<u64> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|_| invalid())
    } else {
        s.parse().map_err(|_| invalid())
    }
}
fn parse(text: &str) -> std::io::Result<Profile> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) { return Err(invalid()); }
    let mut images = BTreeMap::new();
    let mut current: Option<(String, ImageSamples)> = None;
    let mut count = 0usize;
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("image") => {
                if current.is_some() || images.len() >= MAX_IMAGES { return Err(invalid()); }
                let key = fields.next().ok_or_else(invalid)?;
                let size = number(fields.next().ok_or_else(invalid)?)?;
                if fields.next().is_some() || size == 0 || key.is_empty() || key.len() > 160
                    || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    || images.contains_key(key) { return Err(invalid()); }
                current = Some((key.to_owned(), ImageSamples { size, weights: BTreeMap::new() }));
            }
            Some("pc") => {
                let offset = number(fields.next().ok_or_else(invalid)?)?;
                let samples = number(fields.next().ok_or_else(invalid)?)?;
                let (_, image) = current.as_mut().ok_or_else(invalid)?;
                count += 1;
                if fields.next().is_some() || samples == 0 || offset >= image.size
                    || offset & 3 != 0 || count > MAX_ENTRIES
                    || image.weights.insert(offset, samples).is_some() { return Err(invalid()); }
            }
            Some("end") if fields.next().is_none() => {
                let (key, image) = current.take().ok_or_else(invalid)?;
                if image.weights.is_empty() { return Err(invalid()); }
                images.insert(key, image);
            }
            _ => return Err(invalid()),
        }
    }
    if current.is_some() || images.is_empty() { return Err(invalid()); }
    let identity = text.bytes().fold(0xcbf29ce484222325u64,
        |h, byte| (h ^ byte as u64).wrapping_mul(0x100000001b3));
    Ok(Profile { identity, images })
}
fn read_profile() -> std::io::Result<Profile> {
    let path = std::env::var_os("ATTESOR_AOT_LAYOUT_PROFILE")
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "ATTESOR_AOT_LAYOUT_PROFILE is not set"))?;
    // Do not follow a symlink or wait for a FIFO supplied as configuration.
    let file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_BYTES { return Err(invalid()); }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES { return Err(invalid()); }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
    parse(text)
}
fn profile() -> Option<&'static Profile> {
    PROFILE.get_or_init(|| {
        if !enabled() { return None; }
        match read_profile() {
            Ok(profile) => Some(profile),
            Err(e) => {
                eprintln!("[attesor] AOT layout profile ignored ({e}); using original placement");
                None
            }
        }
    }).as_ref()
}

/// Captured from the same immutable snapshot later used by the builder. No
/// path-only identity: replacing a file at the same path invalidates old code.
pub(super) fn identity() -> Option<u64> { profile().map(|p| p.identity) }

/// Order only already discovered/public entries. Sample addresses never create
/// speculative blocks, and there is no rounding an interior PC to a guessed entry.
/// Return the hot prefix length only when an exact image/entry match was found.
pub(super) fn order(img: &Image, blocks: &mut [u64], observed: &BTreeSet<u64>) -> Option<usize> {
    order_with(profile()?, img, blocks, observed)
}

fn order_with(profile: &Profile, img: &Image, blocks: &mut [u64], observed: &BTreeSet<u64>) -> Option<usize> {
    let samples = profile.images.get(&img.key)?;
    if samples.size != img.end.checked_sub(img.start)? { return None; }
    let weight = |pc: u64| pc.checked_sub(img.start)
        .and_then(|offset| samples.weights.get(&offset)).copied().unwrap_or(0);
    let matched = blocks.iter().filter(|&&pc| weight(pc) > 0).count();
    if matched == 0 { return None; }
    // Weighted entries first, then previously observed/unweighted entries, then
    // statically discovered cold entries. Ties use guest PC for deterministic output.
    blocks.sort_unstable_by_key(|&pc| (std::cmp::Reverse(weight(pc)), !observed.contains(&pc), pc));
    let hot = blocks.iter().take_while(|&&pc| weight(pc) > 0 || observed.contains(&pc)).count();
    if crate::jit::stats_enabled() {
        eprintln!("[attesor] aot weighted layout: {} matched={} hot={} cold={}",
            img.name, matched, hot, blocks.len() - hot);
    }
    Some(hot)
}

/// Default placement stays byte-for-byte unchanged. Weighted placement keeps
/// hot/cold assembler units separate; no public index or file format changes.
pub(super) fn chunks(blocks: &[u64], hot: Option<usize>) -> Vec<&[u64]> {
    match hot {
        Some(hot) => blocks[..hot].chunks(1024).chain(blocks[hot..].chunks(1024)).collect(),
        None => blocks.chunks(4096).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> Image {
        Image { name: "test".into(), key: "test-key".into(), start: 0x1000,
            end: 0x3000, source: (0x1000, 0x3000), entries: vec![], loaded: false }
    }
    const TEXT: &str = "Attesor AOT layout samples v1\nimage test-key 8192\npc 0 2\npc 4 9\npc 8 9\npc 100 99\nend\n";
    #[test]
    fn weighted_order_exact_entries_and_sensitivity() {
        let p = parse(TEXT).unwrap();
        let mut blocks = vec![0x1010, 0x1000, 0x1008, 0x100c, 0x1004];
        let original = blocks.clone();
        let observed = BTreeSet::from([0x100c]);
        assert_eq!(order_with(&p, &image(), &mut blocks, &observed), Some(4));
        assert_eq!(blocks, [0x1004, 0x1008, 0x1000, 0x100c, 0x1010]);
        assert_eq!(blocks.iter().copied().collect::<BTreeSet<_>>(), original.iter().copied().collect());
        // Causal control: ignoring samples must fail the expected order, and
        // changing a weight must change both placement and snapshot identity.
        let mut mutant = original; mutant.sort_unstable();
        assert_ne!(mutant, blocks, "no-op layout mutation must be detected");
        let changed = parse(&TEXT.replace("pc 0 2", "pc 0 20")).unwrap();
        assert_ne!(p.identity, changed.identity);
        order_with(&changed, &image(), &mut blocks, &observed);
        assert_eq!(blocks[0], 0x1000);
        let mut wrong = image(); wrong.end += 4;
        assert_eq!(order_with(&p, &wrong, &mut blocks, &observed), None);
        wrong = image(); wrong.key = "other".into();
        assert_eq!(order_with(&p, &wrong, &mut blocks, &observed), None);
        let mut absent = [0x1010];
        assert_eq!(order_with(&p, &image(), &mut absent, &observed), None);
        // Offsets remain meaningful when the entire fixed-address image slides.
        let mut slid = image(); slid.start += 0x10000; slid.end += 0x10000;
        let mut pcs = [0x11000, 0x11004];
        order_with(&p, &slid, &mut pcs, &BTreeSet::new());
        assert_eq!(pcs, [0x11004, 0x11000]);
    }
    #[test]
    fn parser_rejects_bad_records_and_chunk_boundary_is_exact() {
        for text in [TEXT.replace("end", ""), TEXT.replace("pc 0 2", "pc 0 0"),
            TEXT.replace("pc 0 2", "pc 1 2"), TEXT.replace("pc 0 2", "pc 8192 2"),
            TEXT.replace("pc 0 2", "pc 0 2\npc 0 3"),
            TEXT.replace("end", "end\nimage test-key 8192\npc 0 1\nend"),
            TEXT.replace("pc 0 2", "pc 0 18446744073709551616"),
            TEXT.replace("end", "unknown"), TEXT.replace("test-key", "bad_key")] {
            assert!(parse(&text).is_err(), "accepted {text}");
        }
        let blocks: Vec<_> = (0..2050).collect();
        assert_eq!(chunks(&blocks, None).iter().map(|c| c.len()).collect::<Vec<_>>(), [2050]);
        assert_eq!(chunks(&blocks, Some(1025)).iter().map(|c| c.len()).collect::<Vec<_>>(), [1024,1,1024,1]);
        assert_eq!(chunks(&[], Some(0)).len(), 0);
    }
}
