//! Static recompiler: turns the game's PowerPC ELF into Rust source for the `game` crate.
//!
//! usage: recomp <game.elf> <hle.txt> <out_dir>

mod elf;
mod emit;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use elf::{Elf, STT_FUNC, STT_NOTYPE};
use emit::{emit_function, Func, Stats};

/// Instructions per generated source file; keeps each module a reasonable unit for rustc.
const INSNS_PER_MODULE: usize = 12_000;

struct HleRules {
    prefixes: Vec<String>,
    exact: BTreeSet<String>,
}

impl HleRules {
    fn load(path: &str) -> HleRules {
        let mut rules = HleRules { prefixes: Vec::new(), exact: BTreeSet::new() };
        for line in fs::read_to_string(path).expect("cannot read HLE list").lines() {
            let line = line.split('#').next().unwrap().trim();
            match line.strip_prefix('=') {
                Some(name) => drop(rules.exact.insert(name.to_string())),
                None if !line.is_empty() => rules.prefixes.push(line.to_string()),
                None => {}
            }
        }
        rules
    }

    fn matches(&self, name: &str) -> bool {
        if self.exact.contains(name) {
            return true;
        }
        // CodeWarrior C++ mangling appends `__` plus a signature; SDK functions are plain C
        let mangled = name
            .match_indices("__")
            .any(|(i, _)| i > 0 && name[i + 2..].starts_with(|ch: char| ch.is_ascii_digit() || "FQC".contains(ch)));
        if mangled {
            return false;
        }
        let bare = name.trim_start_matches('_');
        self.prefixes.iter().any(|p| {
            bare.strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'))
        })
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: recomp <game.elf> <hle.txt> <out_dir>");
        std::process::exit(2);
    }
    let elf = Elf::parse(fs::read(&args[1]).expect("cannot read ELF"));
    let rules = HleRules::load(&args[2]);
    let out_dir = Path::new(&args[3]);

    // Functions, keyed by address. Several symbols can alias one address; the first wins.
    let mut funcs: BTreeMap<u32, Func> = BTreeMap::new();
    let mut hle_names: Vec<String> = Vec::new();
    for s in &elf.syms {
        if s.ty == STT_FUNC && s.size > 0 && elf.is_exec(s.shndx) && !funcs.contains_key(&s.value) {
            let hle = rules.matches(&s.name).then(|| {
                hle_names.push(s.name.clone());
                hle_names.len() - 1
            });
            funcs.insert(s.value, Func { addr: s.value, size: s.size & !3, name: s.name.clone(), hle });
        }
    }
    // Clip any function whose stated size overlaps its successor.
    let starts: Vec<u32> = funcs.keys().copied().collect();
    let mut clipped = 0;
    for pair in starts.windows(2) {
        let f = funcs.get_mut(&pair[0]).unwrap();
        if f.addr + f.size > pair[1] {
            f.size = pair[1] - f.addr;
            clipped += 1;
        }
    }

    // Some code is entered mid-function (the compiler's register save/restore helpers above
    // all). All guest state lives in `Ctx`, so a function can be split at any instruction:
    // the first half simply falls through into the second. Repeat until every direct branch
    // lands either inside its own function or on a function start.
    let mut splits = 0;
    loop {
        let mut targets = BTreeSet::new();
        for f in funcs.values() {
            for pc in (f.addr..f.addr + f.size).step_by(4) {
                if let Some(t) = elf.word(pc).and_then(|w| emit::branch_target(pc, w)) {
                    if t & 3 == 0 && !(f.addr..f.addr + f.size).contains(&t) && !funcs.contains_key(&t) {
                        targets.insert(t);
                    }
                }
            }
        }
        let mut progress = false;
        for t in targets {
            let Some(owner) = funcs.range_mut(..t).next_back().map(|(_, f)| f).filter(|f| t < f.addr + f.size) else {
                continue; // not inside any known function; reported below
            };
            let tail = Func {
                addr: t,
                size: owner.addr + owner.size - t,
                name: format!("{}+0x{:X}", owner.name, t - owner.addr),
                hle: None,
            };
            owner.size = t - owner.addr;
            funcs.insert(t, tail);
            progress = true;
            splits += 1;
        }
        if !progress {
            break;
        }
    }

    // Pointers into the middle of functions are jump-table targets.
    let data_targets: BTreeSet<u32> =
        elf.addr32_targets().into_iter().filter(|&t| elf.exec_section(t).is_some() && !funcs.contains_key(&t)).collect();

    let mut stats = Stats::default();
    let mut modules: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_insns = 0;
    for f in funcs.values() {
        let words: Vec<u32> = (0..f.size / 4).map(|i| elf.word(f.addr + i * 4).expect("function outside code")).collect();
        emit_function(f, &words, &funcs, &data_targets, &mut stats, &mut cur);
        cur_insns += words.len();
        if cur_insns >= INSNS_PER_MODULE {
            modules.push(std::mem::take(&mut cur));
            cur_insns = 0;
        }
    }
    if !cur.is_empty() {
        modules.push(cur);
    }

    // Start from a clean directory so stale modules never linger.
    let _ = fs::remove_dir_all(out_dir);
    fs::create_dir_all(out_dir).expect("cannot create output directory");
    let mut root = String::from("// Generated by recomp. Do not edit.\n\nuse runtime::{Ctx, GuestFn};\n\n");
    for (i, body) in modules.iter().enumerate() {
        let src = format!("// Generated by recomp. Do not edit.\n\nuse super::*;\n\n{body}");
        fs::write(out_dir.join(format!("m{i:03}.rs")), src).unwrap();
        writeln!(root, "mod m{i:03};\npub use m{i:03}::*;").unwrap();
    }

    writeln!(root, "\n/// Every recompiled function: guest address, native entry point, original symbol.").unwrap();
    writeln!(root, "pub static FUNCS: &[(u32, GuestFn, &str)] = &[").unwrap();
    for f in funcs.values() {
        writeln!(root, "    (0x{:08X}, fn_{:08X}, {:?}),", f.addr, f.addr, f.name).unwrap();
    }
    writeln!(root, "];\n\n/// Names of replaceable SDK functions, indexed by the id passed to `Ctx::hle`.").unwrap();
    writeln!(root, "pub static HLE_NAMES: &[&str] = &[").unwrap();
    for n in &hle_names {
        writeln!(root, "    {n:?},").unwrap();
    }
    writeln!(root, "];\n\n/// Linker-defined symbols (stack top, small-data bases, section bounds).").unwrap();
    writeln!(root, "pub static LINKER_SYMS: &[(&str, u32)] = &[").unwrap();
    for s in elf.syms.iter().filter(|s| s.ty == STT_NOTYPE && !s.name.is_empty()) {
        writeln!(root, "    ({:?}, 0x{:08X}),", s.name, s.value).unwrap();
    }
    writeln!(root, "];").unwrap();
    fs::write(out_dir.join("mod.rs"), root).unwrap();

    let code: u32 = funcs.values().map(|f| f.size).sum();
    println!("functions      : {} ({} KB of code, {} sizes clipped, {} mid-function entries split)", funcs.len(), code / 1024, clipped, splits);
    println!("instructions   : {}", stats.insns);
    println!("state machines : {} functions ({} straight-line)", stats.looped, funcs.len() - stats.looped);
    println!("jump-table refs: {}", data_targets.len());
    println!("replaceable SDK: {} functions", hle_names.len());
    println!("fall off end   : {} functions", stats.fallthrough);
    println!("modules written: {} in {}", modules.len(), out_dir.display());
    println!("untranslatable instructions: {}", stats.invalid.len());
    for (pc, w) in stats.invalid.iter().take(20) {
        println!("  {pc:08X}: {w:08X}  (op {} xo {})", w >> 26, (w >> 1) & 0x3FF);
    }
    println!("branches to non-function addresses: {}", stats.bad_branches.len());
    for (pc, t) in stats.bad_branches.iter().take(20) {
        let owner = funcs.range(..=*t).next_back().map(|(_, f)| f.name.as_str()).unwrap_or("?");
        println!("  {pc:08X} -> {t:08X}  (inside {owner})");
    }
}
