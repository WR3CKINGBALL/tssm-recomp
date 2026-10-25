//! Native DVD library: serves the disc filesystem from an extracted disc directory.
//!
//! Guest structures (offsets in bytes):
//!   DVDCommandBlock: state @0x0C, transferredSize @0x20, callback @0x28
//!   DVDFileInfo:     DVDCommandBlock @0x00, startAddr @0x30, length @0x34, callback @0x38

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use runtime::hle::Hle;
use runtime::Ctx;

use crate::host::{self, Event};

const STATE_BUSY: u32 = 1;

struct Entry {
    host_path: PathBuf,
    offset: u32,
    size: u32,
    is_dir: bool,
}

pub struct Dvd {
    /// Indexed by FST entry number, exactly as on the disc.
    entries: Vec<Entry>,
    /// Lowercased disc path -> entry number.
    by_path: HashMap<String, usize>,
    /// Disc offset -> entry number; file infos identify their file by start offset.
    by_offset: HashMap<u32, usize>,
    open: HashMap<usize, File>,
    /// Size of the disc's file table in bytes.
    table_size: u32,
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").trim_matches('/').to_ascii_lowercase()
}

impl Dvd {
    pub fn open(disc: &Path) -> Dvd {
        let fst = std::fs::read(disc.join("sys/fst.bin")).expect("cannot read sys/fst.bin");
        let be = |o: usize| u32::from_be_bytes(fst[o..o + 4].try_into().unwrap());
        let count = be(8) as usize;
        let names = count * 12;
        let root = disc.join("files");

        let mut dvd = Dvd {
            entries: vec![Entry { host_path: root.clone(), offset: 0, size: 0, is_dir: true }],
            by_path: HashMap::from([(String::new(), 0)]),
            by_offset: HashMap::new(),
            open: HashMap::new(),
            table_size: fst.len() as u32,
        };
        // (index one past the directory's last entry, disc path prefix)
        let mut dirs: Vec<(usize, String)> = vec![(count, String::new())];
        for i in 1..count {
            while i >= dirs.last().unwrap().0 {
                dirs.pop();
            }
            let o = names + (be(i * 12) & 0xFF_FFFF) as usize;
            let len = fst[o..].iter().position(|&b| b == 0).unwrap();
            let name: String = fst[o..o + len].iter().map(|&b| b as char).collect();
            let path = format!("{}{name}", dirs.last().unwrap().1);
            let is_dir = fst[i * 12] == 1;
            dvd.by_path.insert(path.to_ascii_lowercase(), i);
            if is_dir {
                dirs.push((be(i * 12 + 8) as usize, format!("{path}/")));
            } else {
                dvd.by_offset.insert(be(i * 12 + 4), i);
            }
            dvd.entries.push(Entry { host_path: root.join(&path), offset: be(i * 12 + 4), size: be(i * 12 + 8), is_dir });
        }
        dvd
    }

    pub fn table_size(&self) -> u32 {
        self.table_size
    }

    fn lookup(&self, path: &str) -> Option<usize> {
        self.by_path.get(&normalize(path)).copied()
    }

    /// Reads from the file starting at disc offset `start`; bytes past the end read as zero.
    fn read(&mut self, start: u32, offset: u32, buf: &mut [u8]) {
        buf.fill(0);
        let Some(&i) = self.by_offset.get(&start) else {
            eprintln!("[dvd] read from unknown file at disc offset {start:#x}");
            return;
        };
        let entry = &self.entries[i];
        let file = self.open.entry(i).or_insert_with(|| {
            File::open(&entry.host_path).unwrap_or_else(|e| panic!("cannot open {}: {e}", entry.host_path.display()))
        });
        let avail = (entry.size.saturating_sub(offset) as usize).min(buf.len());
        file.seek(SeekFrom::Start(offset as u64)).unwrap();
        file.read_exact(&mut buf[..avail]).unwrap();
        if entry.host_path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("SB04.ini")) {
            boot_override(buf);
        }
        if entry.host_path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("mnus_US.hip")) {
            autosave_notice(&entry.host_path, offset as usize, &mut buf[..avail]);
        }
    }
}

/// TSSM_BOOT=B101 starts the game directly in that level, skipping the menus. Level names
/// are the four-character HIP file names under disc/files. Two things make it happen: the
/// game's SB04.ini is changed as it is read, so that no menu is shown first, and the first
/// level the game then loads is swapped for the named one (`before_scene_init`).
fn boot_override(ini: &mut [u8]) {
    if std::env::var_os("TSSM_BOOT").is_none() {
        return;
    }
    // The game reads the file more than once and in pieces; change whichever piece has it.
    let (from, to) = (b"ShowMenuOnBoot = 1", b"ShowMenuOnBoot = 0");
    if let Some(at) = ini.windows(from.len()).position(|w| w == from) {
        ini[at..at + to.len()].copy_from_slice(to);
    }
}

/// The notice New Game shows before offering to create a save file tells the player not to
/// remove the Memory Card or power off the Nintendo GameCube. It is reworded for a computer
/// or phone as the main menu's file (MN/mnus_US.hip) is read; the file on disk is untouched.
/// A text in that file is its length, then its characters, then a zero; the new one is
/// shorter and the room left over is filled with zeros.
fn autosave_notice(path: &Path, offset: usize, piece: &mut [u8]) {
    const REST: &str = "{n}{n}Would you like to create a game file to save your progress?";
    const OLD: &str = "The game uses an autosave feature. While playing, Do not remove the Memory Card or power off the \
        Nintendo GameCube.";
    const NEW: &str = "The game uses an autosave feature. While playing, do not power off your device or close the game.";
    let (old, new) = (format!("{OLD}{REST}").into_bytes(), format!("{NEW}{REST}").into_bytes());
    /// Where the text's length is in the file, if the file has the text.
    static AT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let at = *AT.get_or_init(|| {
        let file = std::fs::read(path).unwrap_or_default();
        let text = file.windows(old.len()).position(|w| w == old)?;
        let length = text.checked_sub(4)?;
        (file[length..text] == (old.len() as u32).to_be_bytes()).then_some(length)
    });
    let Some(at) = at else { return };
    let mut patch = (new.len() as u32).to_be_bytes().to_vec();
    patch.extend_from_slice(&new);
    patch.resize(4 + old.len(), 0);
    // The game reads the file in pieces; change the part of the text that is in this one.
    for (i, &byte) in patch.iter().enumerate() {
        if let Some(slot) = (at + i).checked_sub(offset).and_then(|j| piece.get_mut(j)) {
            *slot = byte;
        }
    }
}

/// Runs before zSceneInit(sceneID, reloadInProgress), where the game starts loading a level.
/// The ID is the level's four-character name. Each one is logged, which also shows in a log
/// which level a crash happened in.
pub fn before_scene_init(c: &mut Ctx) {
    static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    let first = FIRST.swap(false, std::sync::atomic::Ordering::Relaxed);
    if let (true, Ok(level)) = (first, std::env::var("TSSM_BOOT")) {
        if let Ok(name) = <[u8; 4]>::try_from(level.to_ascii_uppercase().as_bytes()) {
            c.r[3] = u32::from_be_bytes(name);
        }
    }
    eprintln!("[scene] loading {}", String::from_utf8_lossy(&c.r[3].to_be_bytes()));
}

fn cstr(c: &Ctx, mut addr: u32) -> String {
    let mut s = String::new();
    loop {
        match c.r8(addr) {
            0 => return s,
            b => s.push(b as char),
        }
        addr += 1;
    }
}

fn convert_path_to_entrynum(c: &mut Ctx) {
    let path = cstr(c, c.r[3]);
    let found = host::with(|h| h.dvd.lookup(&path));
    if found.is_none() {
        eprintln!("[dvd] no such file: {path}");
    }
    c.r[3] = found.map_or(u32::MAX, |i| i as u32);
}

fn open_entry(c: &mut Ctx, entry: Option<usize>, info: u32) {
    let file = entry.and_then(|i| host::with(|h| h.dvd.entries.get(i).filter(|e| !e.is_dir).map(|e| (e.offset, e.size))));
    match file {
        Some((offset, size)) => {
            c.w32(info + 0x0C, 0);
            c.w32(info + 0x30, offset);
            c.w32(info + 0x34, size);
            c.w32(info + 0x38, 0);
            c.r[3] = 1;
        }
        None => c.r[3] = 0,
    }
}

fn fast_open(c: &mut Ctx) {
    let (entry, info) = (c.r[3], c.r[4]);
    open_entry(c, (entry != u32::MAX).then_some(entry as usize), info);
}

fn open(c: &mut Ctx) {
    let (path, info) = (cstr(c, c.r[3]), c.r[4]);
    let entry = host::with(|h| h.dvd.lookup(&path));
    if entry.is_none() {
        eprintln!("[dvd] no such file: {path}");
    }
    open_entry(c, entry, info);
}

/// DVDReadAsyncPrio(fileInfo, addr, length, offset, callback, prio). The data is copied
/// immediately; the completion callback is delivered later by the event pump.
fn read_async(c: &mut Ctx) {
    let (info, addr, length, offset, callback) = (c.r[3], c.r[4], c.r[5], c.r[6], c.r[7]);
    let start = c.r32(info + 0x30);
    let buf = c.slice_mut(addr, length as usize);
    host::with(|h| {
        h.dvd.read(start, offset, buf);
        h.events.push_back(Event::DvdDone { block: info, callback, result: length as i32 });
    });
    c.w32(info + 0x0C, STATE_BUSY);
    c.w32(info + 0x20, length);
    c.w32(info + 0x38, callback);
    c.r[3] = 1;
}

/// DVDSeekAsyncPrio(fileInfo, offset, callback, prio): nothing to move, just complete.
fn seek_async(c: &mut Ctx) {
    let (info, callback) = (c.r[3], c.r[5]);
    host::with(|h| h.events.push_back(Event::DvdDone { block: info, callback, result: 0 }));
    c.w32(info + 0x0C, STATE_BUSY);
    c.r[3] = 1;
}

fn command_block_status(c: &mut Ctx) {
    let block = c.r[3];
    host::pump(c);
    c.r[3] = c.r32(block + 0x0C);
}

pub fn policy(name: &str) -> Option<Hle> {
    Some(Hle::Native(match name {
        "DVDInit" | "DVDReset" | "DVDResume" | "DVDSetAutoInvalidation" => |_| {},
        "DVDConvertPathToEntrynum" => convert_path_to_entrynum,
        "DVDFastOpen" => fast_open,
        "DVDOpen" => open,
        "DVDClose" => |c| c.r[3] = 1,
        "DVDReadAsyncPrio" => read_async,
        "DVDSeekAsyncPrio" => seek_async,
        "DVDGetCommandBlockStatus" => command_block_status,
        // Drive is always idle with the right disc in it.
        "DVDGetDriveStatus" => |c| {
            host::pump(c);
            c.r[3] = 0;
        },
        "DVDCheckDisk" => |c| {
            host::pump(c);
            c.r[3] = 1;
        },
        "DVDGetCurrentDiskID" => |c| c.r[3] = 0x8000_0000,
        "DVDCancel" | "DVDCancelAsync" | "DVDCancelAllAsync" => |c| c.r[3] = 0,
        _ => return None,
    }))
}
