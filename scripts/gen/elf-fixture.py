#!/usr/bin/env python3
"""Generate the minimal ELF64 fixtures used by the file(RPATH_*)/READ_ELF
differential tests. Each file is a valid ET_DYN/ET_EXEC ELF object with a
hand-built .dynamic section and .dynstr string table; cmELF only needs
section headers to find them, so no program headers or other sections exist.

Usage: elf-fixture.py <output> <kind>
  rpath       DT_RPATH "/first/lib:/second/lib" + 8 pad bytes, trailing string
  runpath     DT_RUNPATH "/first/lib:/second/lib" + 8 pad bytes
  both        DT_RPATH "/r/lib" and DT_RUNPATH "/run/lib" (different strings)
  shared      DT_RPATH and DT_RUNPATH referencing the same string "/same/lib"
  nodyn       ET_EXEC with no SHT_DYNAMIC section
  nopath      ET_DYN with .dynamic lacking RPATH/RUNPATH
  rel         ET_REL relocatable (rpath tags present but type hides them)
  msb         ELF64 big-endian (MSB) with DT_RPATH
  elf32       ELF32 little-endian with DT_RPATH
  trunc       ELF64 truncated inside the section headers
"""
import struct
import sys

EHDR_SIZE = 64
SHDR_SIZE = 64
DYN_SIZE = 16

SHT_DYNAMIC = 6
SHT_STRTAB = 3
DT_NULL = 0
DT_NEEDED = 1
DT_RPATH = 15
DT_SONAME = 14
DT_RUNPATH = 29
ET_EXEC = 2
ET_DYN = 3
ET_REL = 1


def build(e_type, dyn_entries, dynstr, is64=True, little=True):
    """Assemble the ELF image.

    Sections are: index 0 null, 1 .dynamic (when present, sh_link=2),
    2 .dynstr.
    """
    """Assemble the ELF image.

    Sections are: index 0 null, 1 .dynamic (when present, sh_link=2),
    2 .dynstr.
    """
    en = "<" if little else ">"
    ehdr_size = EHDR_SIZE if is64 else 52
    shdr_size = SHDR_SIZE if is64 else 40
    dyn_size = DYN_SIZE if is64 else 8
    ident = (
        b"\x7fELF"
        + bytes([2 if is64 else 1, 1 if little else 2, 1, 0])
        + bytes(8)
    )

    shdrs = [(0, 0, 0, 0, 0)]  # null shdr
    body = bytearray(ehdr_size)  # ehdr placeholder
    cur = ehdr_size
    if dyn_entries:
        fmt = en + ("qQ" if is64 else "iI")
        data = b"".join(struct.pack(fmt, t, v) for t, v in dyn_entries)
        shdrs.append((SHT_DYNAMIC, cur, len(data), 2, dyn_size))
        body += data
        cur += len(data)
    shdrs.append((SHT_STRTAB, cur, len(dynstr), 0, 0))
    body += dynstr
    cur += len(dynstr)
    e_shoff = cur

    def shdr(sh_type, sh_offset, sh_size, sh_link, sh_entsize):
        if is64:
            return struct.pack(
                en + "IIQQQQIIQQ",
                0,
                sh_type,
                0,
                0,
                sh_offset,
                sh_size,
                sh_link,
                0,
                8,
                sh_entsize,
            )
        return struct.pack(
            en + "IIIIIIIIII",
            0,
            sh_type,
            0,
            0,
            sh_offset,
            sh_size,
            sh_link,
            0,
            8,
            sh_entsize,
        )

    if is64:
        ehdr = struct.pack(
            en + "16sHHIQQQIHHHHHH",
            ident,
            e_type,
            62,
            1,
            0,
            0,
            e_shoff,
            0,
            ehdr_size,
            0,
            0,
            shdr_size,
            len(shdrs),
            0,
        )
    else:
        ehdr = struct.pack(
            en + "16sHHIIIIIHHHHHH",
            ident,
            e_type,
            3,  # EM_386
            1,
            0,
            0,
            e_shoff,
            0,
            ehdr_size,
            0,
            0,
            shdr_size,
            len(shdrs),
            0,
        )
    body[: len(ehdr)] = ehdr
    for st, so, ss, sl, se in shdrs:
        body += shdr(st, so, ss, sl, se)
    return bytes(body)


def main():
    out_path, kind = sys.argv[1], sys.argv[2]

    def strs(*items):
        """items: (name, bytes, pad) -> (dynstr, {name: offset})"""
        tab = bytearray(b"\x00")
        offs = {}
        for name, text, pad in items:
            offs[name] = len(tab)
            tab += text + b"\x00" + b"\x00" * pad
        return bytes(tab), offs

    if kind == "rpath":
        dynstr, o = strs(("r", b"/first/lib:/second/lib", 8), ("n", b"NEXT", 0))
        entries = [(DT_RPATH, o["r"]), (DT_NEEDED, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr)
    elif kind == "runpath":
        dynstr, o = strs(("r", b"/first/lib:/second/lib", 8), ("n", b"NEXT", 0))
        entries = [(DT_RUNPATH, o["r"]), (DT_NEEDED, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr)
    elif kind == "both":
        dynstr, o = strs(("r", b"/r/lib", 0), ("u", b"/run/lib", 0), ("n", b"NEXT", 0))
        entries = [
            (DT_RPATH, o["r"]),
            (DT_RUNPATH, o["u"]),
            (DT_NEEDED, o["n"]),
            (DT_NULL, 0),
        ]
        img = build(ET_DYN, entries, dynstr)
    elif kind == "shared":
        dynstr, o = strs(("s", b"/same/lib", 0), ("n", b"NEXT", 0))
        entries = [
            (DT_RPATH, o["s"]),
            (DT_RUNPATH, o["s"]),
            (DT_NEEDED, o["n"]),
            (DT_NULL, 0),
        ]
        img = build(ET_DYN, entries, dynstr)
    elif kind == "nodyn":
        dynstr, _ = strs(("n", b"NEXT", 0))
        img = build(ET_EXEC, [], dynstr)
    elif kind == "nopath":
        dynstr, o = strs(("n", b"libx.so", 0))
        entries = [(DT_NEEDED, o["n"]), (DT_SONAME, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr)
    elif kind == "rel":
        dynstr, o = strs(("r", b"/r/lib", 0))
        entries = [(DT_RPATH, o["r"]), (DT_NULL, 0)]
        img = build(ET_REL, entries, dynstr)
    elif kind == "msb":
        dynstr, o = strs(("r", b"/first/lib:/second/lib", 8), ("n", b"NEXT", 0))
        entries = [(DT_RPATH, o["r"]), (DT_NEEDED, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr, little=False)
    elif kind == "elf32":
        dynstr, o = strs(("r", b"/first/lib:/second/lib", 8), ("n", b"NEXT", 0))
        entries = [(DT_RPATH, o["r"]), (DT_NEEDED, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr, is64=False)
    elif kind == "trunc":
        dynstr, o = strs(("r", b"/first/lib:/second/lib", 8), ("n", b"NEXT", 0))
        entries = [(DT_RPATH, o["r"]), (DT_NEEDED, o["n"]), (DT_NULL, 0)]
        img = build(ET_DYN, entries, dynstr)[:-40]
    else:
        sys.exit(f"unknown kind {kind}")

    with open(out_path, "wb") as f:
        f.write(img)


if __name__ == "__main__":
    main()
