//! Minimal big-endian ELF32 reader: sections, symbols and RELA relocations.

pub struct Section {
    pub ty: u32,
    pub flags: u32,
    pub addr: u32,
    pub off: u32,
    pub size: u32,
    pub link: u32,
}

pub struct Sym {
    pub name: String,
    pub value: u32,
    pub size: u32,
    pub ty: u8,
    pub shndx: u16,
}

pub struct Elf {
    pub data: Vec<u8>,
    pub secs: Vec<Section>,
    pub syms: Vec<Sym>,
}

pub const SHT_SYMTAB: u32 = 2;
pub const SHT_RELA: u32 = 4;
pub const SHT_PROGBITS: u32 = 1;
pub const SHF_EXECINSTR: u32 = 4;
pub const STT_NOTYPE: u8 = 0;
pub const STT_FUNC: u8 = 2;
pub const R_PPC_ADDR32: u32 = 1;

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(d[o..o + 4].try_into().unwrap())
}

fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(d[o..o + 2].try_into().unwrap())
}

fn cstr(d: &[u8], o: usize) -> String {
    let end = d[o..].iter().position(|&b| b == 0).unwrap() + o;
    d[o..end].iter().map(|&b| b as char).collect()
}

impl Elf {
    pub fn parse(data: Vec<u8>) -> Elf {
        assert_eq!(&data[..4], b"\x7fELF", "not an ELF file");
        assert_eq!((data[4], data[5]), (1, 2), "expected 32-bit big-endian ELF");
        let shoff = be32(&data, 32) as usize;
        let shentsize = be16(&data, 46) as usize;
        let shnum = be16(&data, 48) as usize;

        let raw: Vec<[u32; 8]> = (0..shnum)
            .map(|i| std::array::from_fn(|k| be32(&data, shoff + i * shentsize + k * 4)))
            .collect();
        let secs: Vec<Section> = raw
            .iter()
            .map(|s| Section {
                ty: s[1],
                flags: s[2],
                addr: s[3],
                off: s[4],
                size: s[5],
                link: s[6],
            })
            .collect();

        let symtab = secs.iter().find(|s| s.ty == SHT_SYMTAB).expect("ELF has no symbol table");
        let symstr = secs[symtab.link as usize].off as usize;
        let syms = (0..symtab.size as usize / 16)
            .map(|i| {
                let o = symtab.off as usize + i * 16;
                Sym {
                    name: cstr(&data, symstr + be32(&data, o) as usize),
                    value: be32(&data, o + 4),
                    size: be32(&data, o + 8),
                    ty: data[o + 12] & 0xF,
                    shndx: be16(&data, o + 14),
                }
            })
            .collect();
        Elf { data, secs, syms }
    }

    pub fn is_exec(&self, shndx: u16) -> bool {
        self.secs
            .get(shndx as usize)
            .is_some_and(|s| s.ty == SHT_PROGBITS && s.flags & SHF_EXECINSTR != 0)
    }

    /// The executable section containing `addr`, if any.
    pub fn exec_section(&self, addr: u32) -> Option<&Section> {
        self.secs.iter().find(|s| {
            s.ty == SHT_PROGBITS && s.flags & SHF_EXECINSTR != 0 && addr >= s.addr && addr < s.addr + s.size
        })
    }

    pub fn word(&self, addr: u32) -> Option<u32> {
        let s = self.exec_section(addr)?;
        Some(be32(&self.data, (s.off + addr - s.addr) as usize))
    }

    /// Targets of every absolute 32-bit relocation: the addresses the program stores as pointers.
    pub fn addr32_targets(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for s in self.secs.iter().filter(|s| s.ty == SHT_RELA) {
            for i in 0..s.size as usize / 12 {
                let o = s.off as usize + i * 12;
                let info = be32(&self.data, o + 4);
                if info & 0xFF == R_PPC_ADDR32 {
                    if let Some(sym) = self.syms.get((info >> 8) as usize) {
                        out.push(sym.value.wrapping_add(be32(&self.data, o + 8)));
                    }
                }
            }
        }
        out
    }
}
