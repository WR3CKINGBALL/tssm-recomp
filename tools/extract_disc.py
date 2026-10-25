"""Extract a GameCube disc image (GCM/ISO) into a directory.

usage: python extract_disc.py <image.iso> <out_dir>

Layout written:
  <out_dir>/sys/boot.bin, bi2.bin, apploader.img, fst.bin, main.dol
  <out_dir>/files/...            (the disc filesystem)
"""
import os
import struct
import sys


def u32(b, o=0):
    return struct.unpack_from(">I", b, o)[0]


def copy(f, off, size, dst):
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    f.seek(off)
    with open(dst, "wb") as out:
        left = size
        while left:
            chunk = f.read(min(left, 1 << 20))
            if not chunk:
                raise EOFError(dst)
            out.write(chunk)
            left -= len(chunk)


def dol_size(f, off):
    f.seek(off)
    h = f.read(0x100)
    return max(u32(h, i * 4) + u32(h, 0x90 + i * 4) for i in range(18))


def main():
    iso, out = sys.argv[1], sys.argv[2]
    with open(iso, "rb") as f:
        hdr = f.read(0x440)
        if u32(hdr, 0x1C) != 0xC2339F3D:
            sys.exit("not a GameCube disc image")
        dol_off, fst_off, fst_size = u32(hdr, 0x420), u32(hdr, 0x424), u32(hdr, 0x428)

        sysdir = os.path.join(out, "sys")
        copy(f, 0, 0x440, os.path.join(sysdir, "boot.bin"))
        copy(f, 0x440, 0x2000, os.path.join(sysdir, "bi2.bin"))
        f.seek(0x2440 + 0x14)
        app_size, trailer = struct.unpack(">II", f.read(8))
        copy(f, 0x2440, 0x20 + app_size + trailer, os.path.join(sysdir, "apploader.img"))
        copy(f, fst_off, fst_size, os.path.join(sysdir, "fst.bin"))
        copy(f, dol_off, dol_size(f, dol_off), os.path.join(sysdir, "main.dol"))

        f.seek(fst_off)
        fst = f.read(fst_size)
        n = u32(fst, 8)
        strtab = n * 12
        stack = [(n, "")]
        count = total = 0
        for i in range(1, n):
            while i >= stack[-1][0]:
                stack.pop()
            e = i * 12
            o = strtab + (u32(fst, e) & 0xFFFFFF)
            name = fst[o:fst.index(b"\0", o)].decode("latin-1")
            path = stack[-1][1] + name
            if fst[e] == 1:
                stack.append((u32(fst, e + 8), path + "/"))
                os.makedirs(os.path.join(out, "files", path), exist_ok=True)
            else:
                size = u32(fst, e + 8)
                copy(f, u32(fst, e + 4), size, os.path.join(out, "files", path))
                count += 1
                total += size
        print(f"extracted {count} files ({total / 1048576:.1f} MB) to {out}")


if __name__ == "__main__":
    main()
