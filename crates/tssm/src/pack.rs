//! Texture packs in Dolphin's format, from the textures folder next to the game: PNG files,
//! loose or in .zip files as downloaded, named for the texture they replace, such as
//! tex1_128x128_m_41a644c00f677645_e1b1e434c1264c4e_9.png: size, "_m" when the texture is
//! sampled with mipmaps, the XXH64 of its data (level 0), the XXH64 of the part of its
//! palette it uses (paletted formats; "$" for any), and its GX format. A replacement may be
//! larger than the original; texture coordinates are normalized, so it draws the same.
//!
//! Nothing of a pack is in the repo or the release: the builder asks for one (the Xbox
//! version's textures) and copies it here.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const FOLDER: &str = "textures";

enum Source {
    File(PathBuf),
    /// An entry of one of `Pack::zips`.
    Zip(usize, String),
}

struct Pack {
    /// By width, height, format and data hash: the palette hash (None for any) and where.
    entries: HashMap<(u32, u32, u32, u64), Vec<(Option<u64>, Source)>>,
    zips: Vec<Mutex<zip::ZipArchive<File>>>,
}

static PACK: OnceLock<Option<Pack>> = OnceLock::new();

/// Whether a pack was found, so that its hashing can be skipped when not.
pub fn active() -> bool {
    PACK.get_or_init(load).is_some()
}

/// The pack's replacement for a texture, as width, height and RGBA8 texels. `data` is the
/// texture's level 0 as in RAM, `palette` its palette as in TMEM (big-endian entries).
pub fn replacement(format: u32, w: u32, h: u32, data: &[u8], palette: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let pack = PACK.get_or_init(load).as_ref()?;
    let hash = xxhash_rust::xxh64::xxh64(data, 0);
    let palette_hash = used_palette(format, data, palette).map(|used| xxhash_rust::xxh64::xxh64(used, 0));
    let found = pack.entries.get(&(w, h, format, hash)).and_then(|candidates| {
        candidates.iter().find(|(want, _)| want.is_none() || *want == palette_hash).map(|(_, source)| source)
    });
    // TSSM_TRACE_PACK=1: each texture's name in the pack's terms, and whether it has one.
    static TRACE: OnceLock<bool> = OnceLock::new();
    if *TRACE.get_or_init(|| std::env::var_os("TSSM_TRACE_PACK").is_some()) {
        let palette = palette_hash.map_or(String::new(), |p| format!("_{p:016x}"));
        let verdict = if found.is_some() { "replaced" } else { "none" };
        eprintln!("[pack] tex1_{w}x{h}_{hash:016x}{palette}_{format}: {verdict}");
    }
    let source = found?;
    let bytes = match source {
        Source::File(path) => std::fs::read(path).ok()?,
        Source::Zip(zip, name) => {
            let mut archive = pack.zips[*zip].lock().unwrap();
            let mut entry = archive.by_name(name).ok()?;
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes).ok()?;
            bytes
        }
    };
    let decoded = decode_png(&bytes);
    if decoded.is_none() {
        eprintln!("[pack] cannot read the replacement for a {w}x{h} texture");
    }
    decoded
}

/// The entries of a paletted texture's palette that it uses, from its lowest index to its
/// highest, as Dolphin hashes them.
fn used_palette<'a>(format: u32, data: &[u8], palette: &'a [u8]) -> Option<&'a [u8]> {
    let indices: Box<dyn Iterator<Item = usize>> = match format {
        8 => Box::new(data.iter().flat_map(|&b| [(b >> 4) as usize, (b & 15) as usize])),
        9 => Box::new(data.iter().map(|&b| b as usize)),
        10 => Box::new(data.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]]) as usize & 0x3FFF)),
        _ => return None,
    };
    let (low, high) = indices.fold((usize::MAX, 0), |(low, high), i| (low.min(i), high.max(i)));
    palette.get(low * 2..(high + 1) * 2)
}

/// Remembers the packs found next to the game that the player did not want.
const DECLINED: &str = "config/textures.txt";

/// A texture pack put next to the game rather than in the textures folder (after building,
/// the builder having been told no): asks whether to use it, and if so moves it into the
/// folder. A no is remembered, so that it is asked once.
pub fn offer(ask: impl Fn(&str) -> bool) {
    let mut declined = std::fs::read_to_string(DECLINED).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(".") else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if !name.to_ascii_lowercase().ends_with(".zip") || declined.lines().any(|line| line.trim() == name) || !is_pack(&path) {
            continue;
        }
        let question = format!(
            "A texture pack is next to the game:\n\n{name}\n\nUse it? The game gets its sharper textures \
             (such as the Xbox version's) and smoother text. It is moved into the textures folder; \
             to stop using it, take it out of there."
        );
        if ask(&question) {
            let target = Path::new(FOLDER).join(&name);
            let moved = std::fs::create_dir_all(FOLDER).and_then(|_| std::fs::rename(&path, &target));
            match moved {
                Ok(()) => eprintln!("[pack] moved {name} into {FOLDER}"),
                Err(e) => eprintln!("[pack] cannot move {name} into {FOLDER}: {e}"),
            }
        } else {
            if declined.is_empty() {
                declined.push_str("# Texture packs next to the game that are not to be used (and not asked about again).\n");
            } else if !declined.ends_with('\n') {
                declined.push('\n');
            }
            declined.push_str(&name);
            declined.push('\n');
            let _ = std::fs::write(DECLINED, &declined);
        }
    }
}

/// Whether a zip holds replacement textures by Dolphin's names.
fn is_pack(path: &Path) -> bool {
    let Ok(archive) = File::open(path).map_err(|_| ()).and_then(|f| zip::ZipArchive::new(f).map_err(|_| ())) else {
        return false;
    };
    let found = archive.file_names().flatten().any(|n| n.rsplit('/').next().is_some_and(|base| parse(base).is_some()));
    found
}

fn load() -> Option<Pack> {
    let mut pack = Pack { entries: HashMap::new(), zips: Vec::new() };
    let mut files = Vec::new();
    collect(Path::new(FOLDER), &mut files);
    for path in files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if name.to_ascii_lowercase().ends_with(".zip") {
            let Ok(mut archive) = File::open(&path).map_err(|_| ()).and_then(|f| zip::ZipArchive::new(f).map_err(|_| ())) else {
                eprintln!("[pack] cannot open {}", path.display());
                continue;
            };
            let index = pack.zips.len();
            for i in 0..archive.len() {
                let Ok(entry) = archive.by_index_raw(i) else { continue };
                let Ok(inner) = entry.name().map(|n| n.to_string()) else { continue };
                if let Some(key) = parse(inner.rsplit('/').next().unwrap_or_default()) {
                    add(&mut pack, key, Source::Zip(index, inner));
                }
            }
            pack.zips.push(Mutex::new(archive));
        } else if let Some(key) = parse(&name) {
            add(&mut pack, key, Source::File(path));
        }
    }
    let count: usize = pack.entries.values().map(Vec::len).sum();
    if count == 0 {
        return None;
    }
    eprintln!("[pack] {count} replacement textures in {FOLDER}");
    Some(pack)
}

fn add(pack: &mut Pack, (key, palette): ((u32, u32, u32, u64), Option<u64>), source: Source) {
    pack.entries.entry(key).or_default().push((palette, source));
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// A replacement's name as its lookup key and palette hash (None for "$" or a direct-color
/// format). Arbitrary-mipmap ("_arb") level 0 files count; further levels ("_mip1") and
/// other files do not.
fn parse(name: &str) -> Option<((u32, u32, u32, u64), Option<u64>)> {
    let stem = name.strip_suffix(".png").or_else(|| name.strip_suffix(".PNG"))?;
    let stem = stem.strip_suffix("_arb").unwrap_or(stem);
    let mut parts = stem.strip_prefix("tex1_")?.split('_').filter(|&p| p != "m");
    let (w, h) = parts.next()?.split_once('x')?;
    let hex = |p: &str| (p.len() == 16).then(|| u64::from_str_radix(p, 16).ok()).flatten();
    let hash = hex(parts.next()?)?;
    let rest: Vec<&str> = parts.collect();
    let (palette, format) = match rest[..] {
        [format] => (None, format),
        ["$", format] => (None, format),
        [palette, format] => (Some(hex(palette)?), format),
        _ => return None,
    };
    Some(((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?, hash), palette))
}

fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = ::png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(::png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    let pixels = &buffer[..info.buffer_size()];
    let rgba = match info.color_type {
        ::png::ColorType::Rgba => pixels.to_vec(),
        ::png::ColorType::Rgb => pixels.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        ::png::ColorType::GrayscaleAlpha => pixels.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        ::png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        ::png::ColorType::Indexed => return None,
    };
    Some((info.width, info.height, rgba))
}
