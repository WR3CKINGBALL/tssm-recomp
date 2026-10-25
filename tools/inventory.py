"""Classify every function in the game ELF by name, to find the boundary between
code we recompile and Dolphin SDK code the native runtime replaces.

usage: python inventory.py <sb04gc_nm.elf>
"""
import re
import struct
import sys
from collections import Counter, defaultdict

e = open(sys.argv[1], "rb").read()
shoff = struct.unpack_from(">I", e, 32)[0]
shentsize, shnum, _ = struct.unpack_from(">HHH", e, 46)
secs = [struct.unpack_from(">10I", e, shoff + i * shentsize) for i in range(shnum)]
symtab = next(s for s in secs if s[1] == 2)
strbase = secs[symtab[6]][4]

funcs = []
for i in range(symtab[5] // 16):
    name, value, size, info, _, _ = struct.unpack_from(">IIIBBH", e, symtab[4] + i * 16)
    if info & 0xF == 2 and size:
        o = strbase + name
        funcs.append((e[o:e.index(b"\0", o)].decode("latin-1"), size))

# Dolphin SDK libraries: these talk to hardware and get replaced by native code.
SDK = [
    ("GX   graphics", r"(__)?GX[A-Z_]"), ("OS   kernel", r"(__)?OS[A-Z_]|__OS|OSf32|PPC[A-Z]|__init_|__start|__flush"),
    ("DVD  disc", r"(__)?DVD[A-Z_]|AlarmHandler|cb[A-Z]"), ("PAD  input", r"(__)?PAD[A-Z_]"),
    ("SI   serial", r"(__)?SI[A-Z_]"), ("VI   video", r"(__)?VI[A-Z_]"),
    ("AX   audio mixer", r"(__)?AX[A-Z_]|MIX[A-Z]|SP[A-Z][a-z]"), ("AI   audio out", r"(__)?AI[A-Z_]"),
    ("DSP  audio dsp", r"(__)?DSP[A-Z_]"), ("AR   aux ram", r"(__)?ARQ?[A-Z_]"),
    ("CARD memcard", r"(__)?CARD[A-Z_]"), ("EXI  expansion", r"(__)?EXI[A-Z_]"),
    ("MTX  math", r"(C_|PS)?(MTX|VEC|QUAT)[A-Z_]|PSMTX|PSVEC"), ("DB/debugger", r"(__)?DB[A-Z_]|TRK|gdev_|usr_put|MW"),
]


def classify(n):
    for lib, pat in SDK:
        if re.match(pat, n):
            return "sdk", lib
    if re.match(r"_?(Rw|Rp|Rt|Rx|_rw|_rp|_rt|_rx|rw|rp|rt)[A-Z_]|_rx|Rw|.*__Q?[0-9]*Rw", n):
        return "recompile", "RenderWare"
    if re.match(r"Bink|bink|Rad|rad|RAD", n):
        return "recompile", "Bink video"
    if re.match(r"x[A-Z]|.*__[0-9]*x[A-Z]|.*__Q[0-9][0-9]*x", n):
        return "recompile", "engine (x*)"
    if re.match(r"z[A-Z]|.*__[0-9]*z[A-Z]|.*__Q[0-9][0-9]*z", n):
        return "recompile", "game (z*)"
    if re.match(r"i[A-Z]", n):
        return "recompile", "platform glue (i*)"
    return "recompile", "other (libc, C++ runtime, unclassified)"


size_by, count_by, sample = Counter(), Counter(), defaultdict(list)
for n, size in funcs:
    k = classify(n)
    size_by[k] += size
    count_by[k] += 1
    sample[k].append((size, n))

total = sum(size_by.values())
for kind in ("recompile", "sdk"):
    sub = sum(v for k, v in size_by.items() if k[0] == kind)
    print(f"\n{kind.upper()}: {sum(c for k, c in count_by.items() if k[0] == kind)} funcs, {sub / 1024:.0f} KB ({100 * sub / total:.1f}%)")
    for k, v in sorted(size_by.items(), key=lambda x: -x[1]):
        if k[0] == kind:
            print(f"  {k[1]:<42} {count_by[k]:>5} funcs {v / 1024:>7.0f} KB")

print("\nlargest unclassified functions:")
for size, n in sorted(sample[("recompile", "other (libc, C++ runtime, unclassified)")], reverse=True)[:50]:
    print(f"  {size:>6} {n}")

for lib in ("GX   graphics", "AX   audio mixer", "DVD  disc", "PAD  input"):
    names = sorted(n for _, n in sample[("sdk", lib)])
    print(f"\n{lib} ({len(names)}): " + " ".join(names))
