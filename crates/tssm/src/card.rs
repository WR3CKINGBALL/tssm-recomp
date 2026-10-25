//! Native memory card library: slot A holds a card whose files are kept in the `saves` folder
//! (relative to the working directory), slot B is empty.
//!
//! Each file on the card is one host file in GCI format, as Dolphin and other tools exchange
//! saves: the card's 64-byte directory entry followed by the file's 8 KB blocks. Nothing of
//! the card's own layout (block map, checksums, the unlock handshake) exists; the SDK's CARD
//! functions are answered directly.
//!
//! Operations finish at once, but the asynchronous ones report it as the console does: the
//! call returns, the slot reads as busy, and the completion callback runs from the event pump.
//!
//! TSSM_NO_CARD=1 leaves both slots empty, as before saving existed.
//! TSSM_TRACE_CARD=1 logs the card calls the game makes and their results.
//!
//! Guest structures (offsets in bytes):
//!   CARDFileInfo: chan @0, fileNo @4, offset @8, length @0xC, iBlock @0x10
//!   CARDStat:     fileName[32] @0, length @0x20, time @0x24, gameName[4] @0x28,
//!                 company[2] @0x2C, bannerFormat @0x2E, iconAddr @0x30, iconFormat @0x34,
//!                 iconSpeed @0x36, commentAddr @0x38, offsetBanner @0x3C,
//!                 offsetBannerTlut @0x40, offsetIcon[8] @0x44, offsetIconTlut @0x64,
//!                 offsetData @0x68

use std::path::PathBuf;
use std::sync::Mutex;

use runtime::hle::Hle;
use runtime::Ctx;

use crate::host::{self, Event};

const READY: i32 = 0;
const BUSY: i32 = -1;
const NOCARD: i32 = -3;
const NOFILE: i32 = -4;
const IOERROR: i32 = -5;
const EXIST: i32 = -7;
const NOENT: i32 = -8;
const INSSPACE: i32 = -9;
const LIMIT: i32 = -11;
const NAMETOOLONG: i32 = -12;
const FATAL_ERROR: i32 = -128;

/// A "Memory Card 251": 16 megabits, 251 blocks of 8 KB for files.
const SIZE_MEGABITS: u32 = 16;
const SECTOR: u32 = 8192;
const BLOCKS: u32 = 251;
const MAX_FILES: usize = 127;
/// Reads may start and end on these boundaries; writes only on whole sectors.
const SEGMENT: u32 = 512;
const NAME_LENGTH: usize = 32;
const FOLDER: &str = "saves";
const NONE: u32 = 0xFFFF_FFFF;

/// The card's directory entry, which is also the header of a GCI file.
const ENTRY: usize = 64;
const E_GAME: usize = 0; // game code (4) and maker code (2)
const E_BANNER_FORMAT: usize = 7;
const E_NAME: usize = 8;
const E_TIME: usize = 40; // seconds since 2000
const E_ICON_ADDR: usize = 44;
const E_ICON_FORMAT: usize = 48;
const E_ICON_SPEED: usize = 50;
const E_PERMISSION: usize = 52;
const E_BLOCKS: usize = 56;
const E_COMMENT_ADDR: usize = 60;

struct File {
    entry: [u8; ENTRY],
    data: Vec<u8>,
    host_path: PathBuf,
}

impl File {
    fn be16(&self, at: usize) -> u16 {
        u16::from_be_bytes(self.entry[at..at + 2].try_into().unwrap())
    }

    fn be32(&self, at: usize) -> u32 {
        u32::from_be_bytes(self.entry[at..at + 4].try_into().unwrap())
    }

    fn name(&self) -> &[u8] {
        let name = &self.entry[E_NAME..E_NAME + NAME_LENGTH];
        &name[..name.iter().position(|&b| b == 0).unwrap_or(NAME_LENGTH)]
    }

    fn save(&self) -> i32 {
        let mut bytes = self.entry.to_vec();
        bytes.extend_from_slice(&self.data);
        let written = std::fs::create_dir_all(FOLDER).and_then(|()| std::fs::write(&self.host_path, bytes));
        match written {
            Ok(()) => READY,
            Err(e) => {
                eprintln!("[card] cannot write {}: {e}", self.host_path.display());
                IOERROR
            }
        }
    }
}

struct Card {
    /// By file number, as the game addresses them.
    files: Vec<Option<File>>,
    /// Outcome of the last operation, or `BUSY` until its completion has been delivered.
    result: i32,
    transferred: u32,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);

/// How far local time is ahead of UTC, in seconds, as the system has it now.
#[cfg(windows)]
fn local_offset() -> i64 {
    /// SYSTEMTIME: year, month, day of week, day, hour, minute, second, millisecond.
    type SystemTime = [u16; 8];
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemTime(time: *mut SystemTime);
        fn GetLocalTime(time: *mut SystemTime);
    }
    let (mut utc, mut local) = ([0u16; 8], [0u16; 8]);
    unsafe {
        GetSystemTime(&mut utc);
        GetLocalTime(&mut local);
    }
    let minutes = |t: &SystemTime| t[4] as i64 * 60 + t[5] as i64;
    // The two can only be on the same day or on neighbouring ones.
    let days = if local[3] == utc[3] { 0 } else if (local[0], local[1], local[3]) > (utc[0], utc[1], utc[3]) { 1 } else { -1 };
    (days * 1440 + minutes(&local) - minutes(&utc)) * 60
}

#[cfg(unix)]
fn local_offset() -> i64 {
    /// struct tm on 64-bit Linux and Android: nine ints, then the offset from UTC and the
    /// zone's name.
    #[repr(C)]
    struct Tm {
        fields: [i32; 9],
        gmtoff: std::ffi::c_long,
        zone: *const u8,
    }
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut tm = Tm { fields: [0; 9], gmtoff: 0, zone: std::ptr::null() };
    if unsafe { localtime_r(&unix, &mut tm) }.is_null() {
        return 0;
    }
    tm.gmtoff as i64
}

/// Local time in seconds since the start of 2000, which is what the console's clock counts.
pub fn now() -> u64 {
    const EPOCH_2000: i64 = 946_684_800;
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    (unix + local_offset() - EPOCH_2000).max(0) as u64
}

fn load() -> Card {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(FOLDER)
        .map(|dir| dir.filter_map(|entry| Some(entry.ok()?.path())).collect())
        .unwrap_or_default();
    paths.retain(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gci")));
    paths.sort();
    let mut files = Vec::new();
    for host_path in paths {
        let bytes = std::fs::read(&host_path).unwrap_or_default();
        let blocks = bytes.get(E_BLOCKS..E_BLOCKS + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as usize);
        if blocks == 0 || bytes.len() != ENTRY + blocks * SECTOR as usize || files.len() == MAX_FILES {
            eprintln!("[card] ignoring {}: not a save file", host_path.display());
            continue;
        }
        files.push(Some(File { entry: bytes[..ENTRY].try_into().unwrap(), data: bytes[ENTRY..].to_vec(), host_path }));
    }
    eprintln!("[card] slot A: {} file(s) in {FOLDER}", files.len());
    files.resize_with(MAX_FILES, || None);
    Card { files, result: READY, transferred: 0 }
}

fn with<R>(f: impl FnOnce(&mut Card) -> R) -> R {
    f(CARD.lock().unwrap().get_or_insert_with(load))
}

/// Whether slot `chan` has a card in it.
fn present(chan: u32) -> bool {
    static EMPTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    chan == 0 && !*EMPTY.get_or_init(|| std::env::var_os("TSSM_NO_CARD").is_some())
}

/// The game's save menu state, in its small data area (offsets from r13): the memory card
/// slot chosen on the "Slot A / Slot B" screen and the answer to the prompt on show, both
/// -1 until the player has chosen.
const MENU_SLOT: u32 = 0xFFFF_8234;
const MENU_ANSWER: u32 = 0xFFFF_8E30;
/// The answer that means "back".
const ANSWER_BACK: u32 = 4;
/// zSaveLoad_DispatchCB(event, float *argument), which hears the menu's buttons, and the
/// event of a memory card slot being chosen (the argument is the slot).
const MENU_DISPATCH: u32 = 0x8010_B3BC;
const EVENT_SLOT_CHOSEN: u32 = 171;
/// Where zSaveLoad_CardPick's call of zSaveLoad_Tick returns to while it waits on the slot
/// screen.
const SLOT_SCREEN_WAIT: u32 = 0x8010_F510;

/// How many of the start-up room check's looks for a save file are still to come.
static ROOM_CHECK_OPENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
use std::sync::atomic::Ordering::Relaxed;

/// Runs before iSGTgtHaveRoomStartup, the check at the start of the game for room on the
/// card. It looks for each of the game's three save files and, when all three are there,
/// answers that there is no room whatever the card has free; the game then opens with
/// "There is insufficient free space on the Memory Card in Slot A" for anyone who has used
/// all three save slots. The third look is told there is no such file, so the check goes by
/// the card's free space instead, of which there is always enough here.
pub fn before_room_check(_c: &mut Ctx) {
    ROOM_CHECK_OPENS.store(3, Relaxed);
}

/// Whether slot A has been chosen for the player since the save or load menu opened.
static SLOT_CHOSEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs before zSaveLoad_SaveLoop and zSaveLoad_LoadLoop: a save or load menu opens.
pub fn before_menu(_c: &mut Ctx) {
    SLOT_CHOSEN.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Runs in place of zSaveLoad_Tick(bool), the menu's frame, when it would be the first frame
/// of the memory card slot screen. There is only ever the one card, in slot A, so the screen
/// is never shown: the first time in a menu, slot A is chosen as if the player had pressed
/// its button, which leads on to the three save slots. When the menu comes to the screen
/// again (the player went back from the save slots, or the card was turned down with a
/// message), "back" is answered instead, which leaves the menu. Every other frame, and
/// everything when there is no card (TSSM_NO_CARD), is the game's own.
pub fn pick_slot_a(c: &mut Ctx) -> bool {
    let small = |c: &Ctx, offset: u32| c.r32(c.r[13].wrapping_add(offset));
    if c.lr != SLOT_SCREEN_WAIT || !present(0) || small(c, MENU_SLOT) != NONE || small(c, MENU_ANSWER) != NONE {
        return false;
    }
    if SLOT_CHOSEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        c.w32(c.r[13].wrapping_add(MENU_ANSWER), ANSWER_BACK);
        return true;
    }
    // The button's handler also measures the card's free space for the prompts, so call it
    // rather than only setting the slot. Its argument is a float on a stack frame of ours.
    let (lr, sp) = (c.lr, c.r[1]);
    c.r[1] = sp - 0x20;
    c.w32(c.r[1], sp);
    c.w32(c.r[1] + 8, 0f32.to_bits());
    c.r[3] = EVENT_SLOT_CHOSEN;
    c.r[4] = c.r[1] + 8;
    c.call(MENU_DISPATCH);
    c.r[1] = sp;
    c.lr = lr;
    true
}

/// TSSM_TRACE_CARD=1 logs every card call that ends in `done` or `done_async`, with its result.
fn trace(result: i32, asynchronous: bool) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("TSSM_TRACE_CARD").is_some()) {
        let name = game::HLE_NAMES.get(runtime::LAST_HLE.load(std::sync::atomic::Ordering::Relaxed)).copied().unwrap_or("?");
        eprintln!("[card] {name} -> {result}{}", if asynchronous { " (callback to follow)" } else { "" });
    }
}

/// Ends a synchronous call.
fn done(c: &mut Ctx, result: i32) {
    trace(result, false);
    with(|card| card.result = result);
    c.r[3] = result as u32;
}

/// Ends an asynchronous call. A failure is returned at once, as the console does for anything
/// it can tell without touching the card; otherwise the call is accepted and `callback`
/// (which may be null) hears the outcome later.
fn done_async(c: &mut Ctx, chan: u32, callback: u32, result: i32) {
    if result < 0 {
        return done(c, result);
    }
    trace(result, true);
    with(|card| card.result = BUSY);
    host::with(|h| h.events.push_back(Event::CardDone { chan, callback, result }));
    c.r[3] = READY as u32;
}

/// Called by the event pump as it delivers a completion.
pub fn completed(result: i32) {
    with(|card| card.result = result);
}

fn guest_name(c: &Ctx, addr: u32) -> Vec<u8> {
    (0..=NAME_LENGTH as u32).map(|i| c.r8(addr + i)).take_while(|&b| b != 0).collect()
}

/// Looks a file up by name; the file must belong to this game, as on the console.
fn find(card: &Card, c: &Ctx, name: &[u8]) -> Option<usize> {
    let game = [0, 1, 2, 3, 4, 5].map(|i| c.r8(0x8000_0000 + i));
    card.files.iter().position(|file| file.as_ref().is_some_and(|f| f.name() == name && f.entry[E_GAME..E_GAME + 6] == game))
}

fn fill_file_info(c: &mut Ctx, info: u32, chan: u32, number: usize) {
    c.w32(info, chan);
    c.w32(info + 4, number as u32);
    c.w32(info + 8, 0);
    c.w32(info + 0xC, 0);
    c.w16(info + 0x10, 5 + number as u16);
}

/// The open file a CARDFileInfo refers to, as (slot, file number).
fn opened(c: &Ctx, info: u32) -> Result<(u32, usize), i32> {
    let (chan, number) = (c.r32(info), c.r32(info + 4) as usize);
    if !present(chan) {
        return Err(NOCARD);
    }
    if number >= MAX_FILES || with(|card| card.files[number].is_none()) {
        return Err(NOFILE);
    }
    Ok((chan, number))
}

/// CARDMount(chan, workArea, detachCallback) and the checks: nothing to do but report the card.
fn probe(c: &mut Ctx) {
    let chan = c.r[3];
    done(c, if present(chan) { READY } else { NOCARD });
}

/// CARDProbeEx(chan, s32* memSize, s32* sectorSize)
fn probe_ex(c: &mut Ctx) {
    let (chan, size, sector) = (c.r[3], c.r[4], c.r[5]);
    if !present(chan) {
        return c.r[3] = NOCARD as u32;
    }
    if size != 0 {
        c.w32(size, SIZE_MEGABITS);
    }
    if sector != 0 {
        c.w32(sector, SECTOR);
    }
    c.r[3] = READY as u32;
}

/// CARDMountAsync(chan, workArea, detachCallback, attachCallback)
fn mount_async(c: &mut Ctx) {
    let (chan, callback) = (c.r[3], c.r[6]);
    done_async(c, chan, callback, if present(chan) { READY } else { NOCARD });
}

/// CARDCheckEx(chan, s32* xferBytes)
fn check_ex(c: &mut Ctx) {
    let (chan, transferred) = (c.r[3], c.r[4]);
    if present(chan) && transferred != 0 {
        c.w32(transferred, 0);
    }
    done(c, if present(chan) { READY } else { NOCARD });
}

/// CARDCheckExAsync(chan, s32* xferBytes, callback)
fn check_ex_async(c: &mut Ctx) {
    let (chan, transferred, callback) = (c.r[3], c.r[4], c.r[5]);
    if present(chan) && transferred != 0 {
        c.w32(transferred, 0);
    }
    done_async(c, chan, callback, if present(chan) { READY } else { NOCARD });
}

fn format(chan: u32) -> i32 {
    if !present(chan) {
        return NOCARD;
    }
    with(|card| {
        for file in card.files.iter_mut().filter_map(Option::take) {
            let _ = std::fs::remove_file(&file.host_path);
        }
    });
    READY
}

/// CARDFreeBlocks(chan, s32* bytesNotUsed, s32* filesNotUsed)
fn free_blocks(c: &mut Ctx) {
    // The room check asks this after its three looks: none are left to come.
    ROOM_CHECK_OPENS.store(0, Relaxed);
    let (chan, bytes, files) = (c.r[3], c.r[4], c.r[5]);
    if !present(chan) {
        return done(c, NOCARD);
    }
    let (used, count) = with(|card| {
        let files = card.files.iter().flatten();
        (files.clone().map(|f| f.be16(E_BLOCKS) as u32).sum::<u32>(), files.count() as u32)
    });
    if bytes != 0 {
        c.w32(bytes, BLOCKS.saturating_sub(used) * SECTOR);
    }
    if files != 0 {
        c.w32(files, MAX_FILES as u32 - count);
    }
    done(c, READY);
}

/// CARDGetEncoding(chan, u16* encode) and CARDGetSectorSize(chan, u32* size)
fn get_encoding(c: &mut Ctx) {
    let (chan, out) = (c.r[3], c.r[4]);
    if present(chan) {
        c.w16(out, 0); // ANSI
    }
    c.r[3] = if present(chan) { READY } else { NOCARD } as u32;
}

fn get_sector_size(c: &mut Ctx) {
    let (chan, out) = (c.r[3], c.r[4]);
    if present(chan) {
        c.w32(out, SECTOR);
    }
    c.r[3] = if present(chan) { READY } else { NOCARD } as u32;
}

/// CARDGetResultCode(chan). Games poll this while an operation is under way, so it is also
/// where the completion gets its chance to be delivered.
fn get_result_code(c: &mut Ctx) {
    let chan = c.r[3];
    host::pump(c);
    c.r[3] = if present(chan) { with(|card| card.result) } else { NOCARD } as u32;
}

/// CARDOpen(chan, fileName, CARDFileInfo*)
fn open(c: &mut Ctx) {
    let (chan, name, info) = (c.r[3], guest_name(c, c.r[4]), c.r[5]);
    c.w32(info, NONE);
    if !present(chan) {
        return done(c, NOCARD);
    }
    // The third of the room check's three looks for a save file finds none (see
    // `before_room_check`).
    if ROOM_CHECK_OPENS.try_update(Relaxed, Relaxed, |left| left.checked_sub(1)) == Ok(1) {
        return done(c, NOFILE);
    }
    match with(|card| find(card, c, &name)) {
        Some(number) => {
            fill_file_info(c, info, chan, number);
            done(c, READY)
        }
        None => done(c, NOFILE),
    }
}

/// CARDFastOpen(chan, fileNo, CARDFileInfo*)
fn fast_open(c: &mut Ctx) {
    let (chan, number, info) = (c.r[3], c.r[4] as usize, c.r[5]);
    c.w32(info, NONE);
    if !present(chan) {
        return done(c, NOCARD);
    }
    if number >= MAX_FILES {
        return done(c, FATAL_ERROR);
    }
    if with(|card| card.files[number].is_none()) {
        return done(c, NOFILE);
    }
    fill_file_info(c, info, chan, number);
    done(c, READY);
}

/// CARDClose(CARDFileInfo*)
fn close(c: &mut Ctx) {
    let info = c.r[3];
    let chan = c.r32(info);
    c.w32(info, NONE);
    c.r[3] = if present(chan) { READY } else { NOCARD } as u32;
}

/// Host file name for a save: the game's codes and the card file name, kept to characters
/// every file system takes.
fn host_path(entry: &[u8; ENTRY], name: &[u8]) -> PathBuf {
    let safe = |bytes: &[u8]| -> String {
        bytes.iter().map(|&b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { b as char } else { '_' }).collect()
    };
    PathBuf::from(FOLDER).join(format!("{}-{}.gci", safe(&entry[E_GAME..E_GAME + 6]), safe(name)))
}

/// CARDCreateAsync(chan, fileName, size, CARDFileInfo*, callback)
fn create_async(c: &mut Ctx) {
    let (chan, name, size, info, callback) = (c.r[3], guest_name(c, c.r[4]), c.r[5], c.r[6], c.r[7]);
    if !present(chan) {
        return done(c, NOCARD);
    }
    if name.len() > NAME_LENGTH {
        return done(c, NAMETOOLONG);
    }
    if size == 0 || size % SECTOR != 0 {
        return done(c, FATAL_ERROR);
    }
    let result = with(|card| {
        if find(card, c, &name).is_some() {
            return Err(EXIST);
        }
        let number = card.files.iter().position(Option::is_none).ok_or(NOENT)?;
        let used: u32 = card.files.iter().flatten().map(|f| f.be16(E_BLOCKS) as u32).sum();
        if used + size / SECTOR > BLOCKS {
            return Err(INSSPACE);
        }
        let mut entry = [0u8; ENTRY];
        for i in 0..6 {
            entry[E_GAME + i] = c.r8(0x8000_0000 + i as u32);
        }
        entry[6] = 0xFF;
        entry[E_NAME..E_NAME + name.len()].copy_from_slice(&name);
        entry[E_TIME..E_TIME + 4].copy_from_slice(&(now() as u32).to_be_bytes());
        entry[E_ICON_ADDR..E_ICON_ADDR + 4].copy_from_slice(&NONE.to_be_bytes());
        entry[E_PERMISSION] = 4; // public
        entry[54..56].copy_from_slice(&(5 + number as u16).to_be_bytes()); // first block
        entry[E_BLOCKS..E_BLOCKS + 2].copy_from_slice(&((size / SECTOR) as u16).to_be_bytes());
        entry[58..60].fill(0xFF);
        entry[E_COMMENT_ADDR..].copy_from_slice(&NONE.to_be_bytes());
        let file = File { entry, data: vec![0; size as usize], host_path: host_path(&entry, &name) };
        match file.save() {
            READY => {
                card.files[number] = Some(file);
                card.transferred = 0;
                Ok(number)
            }
            error => Err(error),
        }
    });
    match result {
        Ok(number) => {
            fill_file_info(c, info, chan, number);
            done_async(c, chan, callback, READY)
        }
        Err(error) => done(c, error),
    }
}

/// CARDDeleteAsync(chan, fileName, callback)
fn delete_async(c: &mut Ctx) {
    let (chan, name, callback) = (c.r[3], guest_name(c, c.r[4]), c.r[5]);
    if !present(chan) {
        return done(c, NOCARD);
    }
    let result = with(|card| match find(card, c, &name) {
        Some(number) => {
            let file = card.files[number].take().unwrap();
            let _ = std::fs::remove_file(&file.host_path);
            READY
        }
        None => NOFILE,
    });
    done_async(c, chan, callback, result);
}

/// The range a read or write covers, checked against the file as the console does.
fn range(number: usize, length: u32, offset: u32, unit: u32) -> Result<std::ops::Range<usize>, i32> {
    if offset % unit != 0 || length % unit != 0 {
        return Err(FATAL_ERROR);
    }
    let size = with(|card| card.files[number].as_ref().map_or(0, |f| f.data.len()));
    let end = offset as usize + length as usize;
    if end > size {
        return Err(LIMIT);
    }
    Ok(offset as usize..end)
}

fn read(c: &mut Ctx) -> (u32, i32) {
    let (info, buffer, length, offset) = (c.r[3], c.r[4], c.r[5], c.r[6]);
    let result = opened(c, info).and_then(|(chan, number)| {
        let range = range(number, length, offset, SEGMENT)?;
        let target = c.slice_mut(buffer, length as usize);
        with(|card| {
            target.copy_from_slice(&card.files[number].as_ref().unwrap().data[range]);
            card.transferred = length;
        });
        Ok(chan)
    });
    match result {
        Ok(chan) => (chan, READY),
        Err(error) => (c.r32(info), error),
    }
}

/// CARDWriteAsync(CARDFileInfo*, buffer, length, offset, callback)
fn write_async(c: &mut Ctx) {
    let (info, buffer, length, offset, callback) = (c.r[3], c.r[4], c.r[5], c.r[6], c.r[7]);
    let result = opened(c, info).and_then(|(chan, number)| {
        let range = range(number, length, offset, SECTOR)?;
        let source = c.read_bytes(buffer, length as usize);
        let saved = with(|card| {
            let file = card.files[number].as_mut().unwrap();
            file.data[range].copy_from_slice(&source);
            file.entry[E_TIME..E_TIME + 4].copy_from_slice(&(now() as u32).to_be_bytes());
            card.transferred = length;
            file.save()
        });
        if saved == READY { Ok(chan) } else { Err(saved) }
    });
    match result {
        Ok(chan) => done_async(c, chan, callback, READY),
        Err(error) => done(c, error),
    }
}

/// CARDGetStatus(chan, fileNo, CARDStat*)
fn get_status(c: &mut Ctx) {
    let (chan, number, stat) = (c.r[3], c.r[4] as usize, c.r[5]);
    if !present(chan) {
        return done(c, NOCARD);
    }
    if number >= MAX_FILES {
        return done(c, FATAL_ERROR);
    }
    let Some(entry) = with(|card| card.files[number].as_ref().map(|f| f.entry)) else { return done(c, NOFILE) };
    let file = File { entry, data: Vec::new(), host_path: PathBuf::new() };
    c.slice_mut(stat, 0x6C).fill(0);
    c.slice_mut(stat, NAME_LENGTH).copy_from_slice(&entry[E_NAME..E_NAME + NAME_LENGTH]);
    c.w32(stat + 0x20, file.be16(E_BLOCKS) as u32 * SECTOR);
    c.w32(stat + 0x24, file.be32(E_TIME));
    c.slice_mut(stat + 0x28, 6).copy_from_slice(&entry[E_GAME..E_GAME + 6]);
    c.w8(stat + 0x2E, entry[E_BANNER_FORMAT]);
    c.w32(stat + 0x30, file.be32(E_ICON_ADDR));
    c.w16(stat + 0x34, file.be16(E_ICON_FORMAT));
    c.w16(stat + 0x36, file.be16(E_ICON_SPEED));
    c.w32(stat + 0x38, file.be32(E_COMMENT_ADDR));
    update_icon_offsets(c, stat);
    done(c, READY);
}

/// Where in the file the banner, the icons and their palettes are, worked out from the
/// formats as the console's library does: they follow one another from `iconAddr`.
fn update_icon_offsets(c: &mut Ctx, stat: u32) {
    let mut offset = c.r32(stat + 0x30);
    if offset == NONE {
        c.w8(stat + 0x2E, 0);
        c.w16(stat + 0x34, 0);
        c.w16(stat + 0x36, 0);
        offset = 0;
    }
    let (banner, banner_palette) = match c.r8(stat + 0x2E) & 3 {
        1 => {
            let at = offset;
            offset += 96 * 32 + 2 * 256;
            (at, at + 96 * 32)
        }
        2 => {
            let at = offset;
            offset += 2 * 96 * 32;
            (at, NONE)
        }
        _ => (NONE, NONE),
    };
    c.w32(stat + 0x3C, banner);
    c.w32(stat + 0x40, banner_palette);
    let formats = c.r16(stat + 0x34);
    let mut palette = false;
    for i in 0..8 {
        let at = match (formats >> (2 * i)) & 3 {
            1 => {
                palette = true;
                offset += 32 * 32;
                offset - 32 * 32
            }
            2 => {
                offset += 2 * 32 * 32;
                offset - 2 * 32 * 32
            }
            _ => NONE,
        };
        c.w32(stat + 0x44 + 4 * i, at);
    }
    c.w32(stat + 0x64, if palette { offset } else { NONE });
    if palette {
        offset += 2 * 256;
    }
    c.w32(stat + 0x68, offset);
}

/// CARDSetStatus(chan, fileNo, CARDStat*): stores the presentation fields of the status.
fn set_status(c: &mut Ctx) -> i32 {
    let (chan, number, stat) = (c.r[3], c.r[4] as usize, c.r[5]);
    if !present(chan) {
        return NOCARD;
    }
    let (icon, comment) = (c.r32(stat + 0x30), c.r32(stat + 0x38));
    if number >= MAX_FILES || (icon != NONE && icon >= SEGMENT) || (comment != NONE && comment % SECTOR > SECTOR - 64) {
        return FATAL_ERROR;
    }
    update_icon_offsets(c, stat);
    if c.r32(stat + 0x30) == NONE {
        let speed = c.r16(stat + 0x36);
        c.w16(stat + 0x36, speed & !3 | 1);
    }
    let (banner_format, icon_format, icon_speed) = (c.r8(stat + 0x2E), c.r16(stat + 0x34), c.r16(stat + 0x36));
    with(|card| {
        let Some(file) = card.files[number].as_mut() else { return NOFILE };
        file.entry[E_BANNER_FORMAT] = banner_format;
        file.entry[E_ICON_ADDR..E_ICON_ADDR + 4].copy_from_slice(&icon.to_be_bytes());
        file.entry[E_ICON_FORMAT..E_ICON_FORMAT + 2].copy_from_slice(&icon_format.to_be_bytes());
        file.entry[E_ICON_SPEED..E_ICON_SPEED + 2].copy_from_slice(&icon_speed.to_be_bytes());
        file.entry[E_COMMENT_ADDR..].copy_from_slice(&comment.to_be_bytes());
        file.entry[E_TIME..E_TIME + 4].copy_from_slice(&(now() as u32).to_be_bytes());
        file.save()
    })
}

/// CARDSetAttributes(chan, fileNo, u8 attributes)
fn set_attributes(c: &mut Ctx) {
    let (chan, number, attributes) = (c.r[3], c.r[4] as usize, c.r[5] as u8);
    if !present(chan) {
        return done(c, NOCARD);
    }
    if number >= MAX_FILES {
        return done(c, FATAL_ERROR);
    }
    let result = with(|card| match card.files[number].as_mut() {
        Some(file) => {
            file.entry[E_PERMISSION] = attributes;
            file.save()
        }
        None => NOFILE,
    });
    done(c, result);
}

pub fn policy(name: &str) -> Option<Hle> {
    Some(Hle::Native(match name {
        "CARDInit" => |_| {},
        "CARDMount" | "CARDCheck" | "CARDUnmount" => probe,
        "CARDProbeEx" => probe_ex,
        "CARDMountAsync" => mount_async,
        "CARDCheckEx" => check_ex,
        "CARDCheckExAsync" => check_ex_async,
        "CARDFormat" => |c| {
            let result = format(c.r[3]);
            done(c, result)
        },
        "CARDFormatAsync" => |c| {
            let (chan, callback) = (c.r[3], c.r[4]);
            done_async(c, chan, callback, format(chan))
        },
        "CARDFreeBlocks" => free_blocks,
        "CARDGetEncoding" => get_encoding,
        "CARDGetSectorSize" => get_sector_size,
        "CARDGetResultCode" => get_result_code,
        "CARDGetXferredBytes" => |c| c.r[3] = if present(c.r[3]) { with(|card| card.transferred) } else { 0 },
        "CARDOpen" => open,
        "CARDFastOpen" => fast_open,
        "CARDClose" => close,
        "CARDCreateAsync" => create_async,
        "CARDDeleteAsync" => delete_async,
        "CARDRead" => |c| {
            let (_, result) = read(c);
            done(c, result)
        },
        "CARDReadAsync" => |c| {
            let callback = c.r[7];
            let (chan, result) = read(c);
            done_async(c, chan, callback, result)
        },
        "CARDWriteAsync" => write_async,
        "CARDGetStatus" => get_status,
        "CARDSetStatus" => |c| {
            let result = set_status(c);
            done(c, result)
        },
        "CARDSetStatusAsync" => |c| {
            let (chan, callback) = (c.r[3], c.r[6]);
            let result = set_status(c);
            done_async(c, chan, callback, result)
        },
        "CARDSetAttributes" => set_attributes,
        // Anything else of the card library is internal to it and never reached.
        _ if name.starts_with("CARD") || name.starts_with("__CARD") => |c| c.r[3] = NOCARD as u32,
        _ => return None,
    }))
}
