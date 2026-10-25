//! Extracts a GameCube disc image (GCM/ISO) into a directory, as tools/extract_disc.py does.
//!
//! usage: extract <image.iso> <out_dir>
//!
//! Layout written:
//!   <out_dir>/sys/boot.bin, bi2.bin, apploader.img, fst.bin, main.dol
//!   <out_dir>/files/...            (the disc filesystem)

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn read_at(f: &mut File, offset: u64, size: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0; size];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn copy(f: &mut File, offset: u64, size: u64, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to.parent().unwrap())?;
    f.seek(SeekFrom::Start(offset))?;
    let mut out = io::BufWriter::new(File::create(to)?);
    let copied = io::copy(&mut Read::by_ref(f).take(size), &mut out)?;
    if copied != size {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} is cut short", to.display())));
    }
    out.flush()
}

/// The size of a DOL executable: the furthest end of its eighteen sections.
fn dol_size(f: &mut File, offset: u64) -> io::Result<u64> {
    let header = read_at(f, offset, 0x100)?;
    Ok((0..18).map(|i| u32_at(&header, i * 4) as u64 + u32_at(&header, 0x90 + i * 4) as u64).max().unwrap())
}

fn extract(image: &Path, out: &Path) -> io::Result<()> {
    let mut f = File::open(image)?;
    let header = read_at(&mut f, 0, 0x440)?;
    if u32_at(&header, 0x1C) != 0xC233_9F3D {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a GameCube disc image"));
    }
    let (dol, fst_offset, fst_size) = (u32_at(&header, 0x420) as u64, u32_at(&header, 0x424) as u64, u32_at(&header, 0x428) as usize);

    let sys = out.join("sys");
    copy(&mut f, 0, 0x440, &sys.join("boot.bin"))?;
    copy(&mut f, 0x440, 0x2000, &sys.join("bi2.bin"))?;
    let apploader = read_at(&mut f, 0x2440 + 0x14, 8)?;
    copy(&mut f, 0x2440, 0x20 + u32_at(&apploader, 0) as u64 + u32_at(&apploader, 4) as u64, &sys.join("apploader.img"))?;
    copy(&mut f, fst_offset, fst_size as u64, &sys.join("fst.bin"))?;
    let size = dol_size(&mut f, dol)?;
    copy(&mut f, dol, size, &sys.join("main.dol"))?;

    // The file system table: twelve bytes an entry (directory flag and name offset, then
    // offset and size, or parent and the index after the directory's last entry), then names.
    let fst = read_at(&mut f, fst_offset, fst_size)?;
    let entries = u32_at(&fst, 8) as usize;
    let names = entries * 12;
    let mut directories: Vec<(usize, PathBuf)> = vec![(entries, out.join("files"))];
    let (mut count, mut total) = (0, 0u64);
    for i in 1..entries {
        while i >= directories.last().unwrap().0 {
            directories.pop();
        }
        let entry = i * 12;
        let start = names + (u32_at(&fst, entry) & 0xFF_FFFF) as usize;
        let end = start + fst[start..].iter().position(|&b| b == 0).unwrap_or(0);
        // Latin-1, as the disc's names are.
        let name: String = fst[start..end].iter().map(|&b| b as char).collect();
        let path = directories.last().unwrap().1.join(&name);
        if fst[entry] == 1 {
            fs::create_dir_all(&path)?;
            directories.push((u32_at(&fst, entry + 8) as usize, path));
        } else {
            let size = u32_at(&fst, entry + 8) as u64;
            copy(&mut f, u32_at(&fst, entry + 4) as u64, size, &path)?;
            count += 1;
            total += size;
        }
    }
    println!("extracted {count} files ({:.1} MB) to {}", total as f64 / 1_048_576.0, out.display());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: extract <image.iso> <out_dir>");
        std::process::exit(2);
    }
    if let Err(error) = extract(Path::new(&args[1]), Path::new(&args[2])) {
        eprintln!("extract: {error}");
        std::process::exit(1);
    }
}
