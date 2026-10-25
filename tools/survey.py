"""Scope survey of the game ELF for the recompiler: which PowerPC instructions are
used, and how the code splits between game, engine, RenderWare and the Dolphin SDK.

usage: python survey.py <sb04gc_nm.elf>
"""
import re
import struct
import sys
from collections import Counter, defaultdict

e = open(sys.argv[1], "rb").read()
shoff = struct.unpack_from(">I", e, 32)[0]
shentsize, shnum, shstrndx = struct.unpack_from(">HHH", e, 46)
secs = [struct.unpack_from(">10I", e, shoff + i * shentsize) for i in range(shnum)]


def cstr(base, o):
    o += base
    return e[o:e.index(b"\0", o)].decode("latin-1")


names = [cstr(secs[shstrndx][4], s[0]) for s in secs]
symtab = next(s for s in secs if s[1] == 2)
strbase = secs[symtab[6]][4]

# symbols, in table order, so each function is attributed to the preceding FILE symbol
funcs = []
cur_file = "?"
for i in range(symtab[5] // 16):
    name, value, size, info, _, shndx = struct.unpack_from(">IIIBBH", e, symtab[4] + i * 16)
    t = info & 0xF
    if t == 4:
        cur_file = cstr(strbase, name)
    elif t == 2 and size:
        funcs.append((value, size, cstr(strbase, name), cur_file, shndx))


def read(addr, size, shndx):
    s = secs[shndx]
    return e[s[4] + addr - s[3]: s[4] + addr - s[3] + size]


PRIMARY = {
    3: "twi", 7: "mulli", 8: "subfic", 10: "cmpli", 11: "cmpi", 12: "addic", 13: "addic.", 14: "addi", 15: "addis",
    16: "bc", 17: "sc", 18: "b", 20: "rlwimi", 21: "rlwinm", 23: "rlwnm", 24: "ori", 25: "oris", 26: "xori",
    27: "xoris", 28: "andi.", 29: "andis.", 32: "lwz", 33: "lwzu", 34: "lbz", 35: "lbzu", 36: "stw", 37: "stwu",
    38: "stb", 39: "stbu", 40: "lhz", 41: "lhzu", 42: "lha", 43: "lhau", 44: "sth", 45: "sthu", 46: "lmw",
    47: "stmw", 48: "lfs", 49: "lfsu", 50: "lfd", 51: "lfdu", 52: "stfs", 53: "stfsu", 54: "stfd", 55: "stfdu",
    56: "psq_l", 57: "psq_lu", 60: "psq_st", 61: "psq_stu",
}
X19 = {0: "mcrf", 16: "bclr", 528: "bcctr", 50: "rfi", 150: "isync", 33: "crnor", 129: "crandc", 193: "crxor",
       225: "crnand", 257: "crand", 289: "creqv", 417: "crorc", 449: "cror"}
X31 = {
    0: "cmp", 4: "tw", 8: "subfc", 10: "addc", 11: "mulhwu", 19: "mfcr", 20: "lwarx", 23: "lwzx", 24: "slw",
    26: "cntlzw", 28: "and", 32: "cmpl", 40: "subf", 54: "dcbst", 55: "lwzux", 60: "andc", 75: "mulhw", 83: "mfmsr",
    86: "dcbf", 87: "lbzx", 104: "neg", 119: "lbzux", 124: "nor", 136: "subfe", 138: "adde", 144: "mtcrf",
    146: "mtmsr", 150: "stwcx.", 151: "stwx", 183: "stwux", 200: "subfze", 202: "addze", 210: "mtsr", 215: "stbx",
    232: "subfme", 234: "addme", 235: "mullw", 246: "dcbtst", 247: "stbux", 266: "add", 278: "dcbt", 279: "lhzx",
    284: "eqv", 306: "tlbie", 311: "lhzux", 316: "xor", 339: "mfspr", 343: "lhax", 371: "mftb", 375: "lhaux",
    407: "sthx", 412: "orc", 439: "sthux", 444: "or", 459: "divwu", 467: "mtspr", 470: "dcbi", 476: "nand",
    491: "divw", 512: "mcrxr", 533: "lswx", 534: "lwbrx", 535: "lfsx", 536: "srw", 567: "lfsux", 595: "mfsr",
    597: "lswi", 598: "sync", 599: "lfdx", 631: "lfdux", 661: "stswx", 662: "stwbrx", 663: "stfsx", 695: "stfsux",
    725: "stswi", 727: "stfdx", 759: "stfdux", 790: "lhbrx", 792: "sraw", 824: "srawi", 854: "eieio", 918: "sthbrx",
    922: "extsh", 954: "extsb", 982: "icbi", 983: "stfiwx", 1014: "dcbz",
}
X59 = {18: "fdivs", 20: "fsubs", 21: "fadds", 22: "fsqrts", 24: "fres", 25: "fmuls", 28: "fmsubs", 29: "fmadds",
       30: "fnmsubs", 31: "fnmadds"}
X63 = {0: "fcmpu", 12: "frsp", 14: "fctiw", 15: "fctiwz", 32: "fcmpo", 38: "mtfsb1", 40: "fneg", 64: "mcrfs",
       70: "mtfsb0", 72: "fmr", 134: "mtfsfi", 136: "fnabs", 264: "fabs", 583: "mffs", 711: "mtfsf"}
A63 = {18: "fdiv", 20: "fsub", 21: "fadd", 22: "fsqrt", 23: "fsel", 25: "fmul", 26: "frsqrte", 28: "fmsub",
       29: "fmadd", 30: "fnmsub", 31: "fnmadd"}
X4 = {0: "ps_cmpu0", 32: "ps_cmpo0", 40: "ps_neg", 64: "ps_cmpu1", 72: "ps_mr", 96: "ps_cmpo1", 136: "ps_nabs",
      264: "ps_abs", 528: "ps_merge00", 560: "ps_merge01", 592: "ps_merge10", 624: "ps_merge11", 1014: "dcbz_l"}
A4 = {10: "ps_sum0", 11: "ps_sum1", 12: "ps_muls0", 13: "ps_muls1", 14: "ps_madds0", 15: "ps_madds1", 18: "ps_div",
      20: "ps_sub", 21: "ps_add", 23: "ps_sel", 24: "ps_res", 25: "ps_mul", 26: "ps_rsqrte", 28: "ps_msub",
      29: "ps_madd", 30: "ps_nmsub", 31: "ps_nmadd"}
X4_6 = {6: "psq_lx", 7: "psq_stx", 38: "psq_lux", 39: "psq_stux"}


def mnemonic(w):
    op = w >> 26
    if op in PRIMARY:
        return PRIMARY[op]
    xo10, xo5 = (w >> 1) & 0x3FF, (w >> 1) & 0x1F
    if op == 19:
        return X19.get(xo10)
    if op == 31:
        return X31.get(xo10) or X31.get(xo10 & 0x1FF if xo10 & 0x200 else -1)  # OE forms
    if op == 59:
        return X59.get(xo5)
    if op == 63:
        return X63.get(xo10) or A63.get(xo5)
    if op == 4:
        return X4.get(xo10) or A4.get(xo5) or X4_6.get((w >> 1) & 0x3F)
    return None


def bucket(name, src):
    s = src.lower()
    if re.match(r"z", s):
        return "game (z*)"
    if re.match(r"x", s):
        return "engine (x*)"
    if re.match(r"i[A-Z]", src):
        return "platform (i*)"
    if re.match(r"(rw|rp|rt|ba|rx|_rw|_rp)", name, re.I) or re.match(r"(ba|rp|rt|rw|rx|pip|nod|d[0-9]|vec|drv|native)", s):
        return "RenderWare"
    return "SDK/libs/runtime"


ops = Counter()
unknown = Counter()
bytes_by = Counter()
count_by = Counter()
files_by = defaultdict(Counter)
spr = Counter()
indirect = Counter()
for addr, size, name, src, shndx in funcs:
    b = bucket(name, src)
    bytes_by[b] += size
    count_by[b] += 1
    files_by[b][src] += size
    code = read(addr, size, shndx)
    for (w,) in struct.iter_unpack(">I", code[: len(code) & ~3]):
        m = mnemonic(w)
        if m is None:
            unknown[f"op{w >> 26}/xo{(w >> 1) & 0x3FF}"] += 1
            continue
        ops[m] += 1
        if m in ("mfspr", "mtspr"):
            n = ((w >> 16) & 0x1F) | ((w >> 6) & 0x3E0)
            spr[f"{m} {n}"] += 1
        if m == "bcctr":
            indirect["bctrl (indirect call)" if w & 1 else "bctr (jump table / tail call)"] += 1

total = sum(ops.values())
print(f"functions: {len(funcs)}   instructions: {total}   distinct mnemonics: {len(ops)}")
print("\ncode by origin:")
for b, n in bytes_by.most_common():
    print(f"  {b:<20} {count_by[b]:>5} funcs  {n / 1024:>7.0f} KB  {100 * n / sum(bytes_by.values()):>5.1f}%")

print("\nSDK/libs/runtime source files (top 60 by size):")
print("  " + ", ".join(f"{f}({n // 1024}K)" for f, n in files_by["SDK/libs/runtime"].most_common(60)))

fam = Counter()
for m, n in ops.items():
    f = ("paired-single" if m.startswith("ps") or m == "dcbz_l" else
         "float" if m[0] == "f" or m in ("lfs", "lfd", "stfs", "stfd", "lfsx", "lfdx", "stfsx", "stfdx", "lfsu", "lfdu",
                                         "stfsu", "stfdu", "lfsux", "lfdux", "stfsux", "stfdux", "stfiwx", "mffs",
                                         "mtfsf", "mtfsb0", "mtfsb1", "mtfsfi", "mcrfs") else
         "privileged/cache" if m in ("mfmsr", "mtmsr", "rfi", "sc", "mtsr", "mfsr", "tlbie", "dcbf", "dcbi", "dcbst",
                                     "dcbt", "dcbtst", "dcbz", "icbi", "sync", "isync", "eieio", "mftb", "mfspr",
                                     "mtspr", "lwarx", "stwcx.") else "integer/branch")
    fam[f] += n
print("\ninstruction families:")
for f, n in fam.most_common():
    print(f"  {f:<18} {n:>8}  {100 * n / total:>5.2f}%")

print("\nall mnemonics:")
print("  " + "  ".join(f"{m}:{n}" for m, n in ops.most_common()))
print("\nindirect branches:", dict(indirect))
print("SPR access:", dict(spr.most_common()))
print(f"\nundecoded words: {sum(unknown.values())}", dict(unknown.most_common(15)))

# which functions touch privileged state — these get replaced by the runtime, never recompiled
priv = Counter()
for addr, size, name, src, shndx in funcs:
    code = read(addr, size, shndx)
    for (w,) in struct.iter_unpack(">I", code[: len(code) & ~3]):
        if mnemonic(w) in ("mfmsr", "mtmsr", "rfi", "sc", "mtsr", "tlbie"):
            priv[src] += 1
            break
print(f"\nfunctions touching MSR/rfi/sc: {sum(priv.values())}, by file:", dict(priv.most_common(25)))
