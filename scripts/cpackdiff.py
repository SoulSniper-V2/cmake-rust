#!/usr/bin/env python3
"""CPack differential test.

Each fixture under tests/cpack/<case> is configured (and optionally built) once
with the pinned Kitware CMake oracle.  The oracle cpack and candidate cpack then
package that same build tree into separate output directories.  The harness
compares exit codes, normalized stdout/stderr, produced file names and package
contents.  Archive timestamps and compressed container bytes are intentionally
not compared; member names, types, modes, ownership metadata, symlink targets
and file bytes are.

Fixture files:
  generators.txt  generator list argument for -G (default TGZ)
  preargs.txt     extra cpack arguments before -G, one shell-style line per line
  args.txt        extra cpack arguments, one shell-style line per line
  build.txt       if present, build the fixture before packaging
  source.txt      if present, use CPackSourceConfig.cmake instead
"""
import argparse
import bz2
import concurrent.futures
import gzip
import hashlib
import lzma
import os
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ORACLE_CMAKE = Path("/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake")
ORACLE_CPACK = ORACLE_CMAKE.parent / "cpack"
DATA_ROOT_TMP = "/tmp/cmake-rust-validation.hSLgEZHR/kitware"
DATA_ROOT = os.path.realpath(DATA_ROOT_TMP)


def run(cmd, cwd=None, env=None, timeout=300):
    return subprocess.run([str(x) for x in cmd], cwd=cwd, env=env, capture_output=True, timeout=timeout)


def norm(text, out):
    text = text.replace(str(out), "<OUT>")
    text = text.replace(DATA_ROOT, "<ROOT>")
    text = text.replace(DATA_ROOT_TMP, "<ROOT>")
    text = re.sub(r"cpack version 4\.1\.0[^\n]*", "cpack version 4.1.0", text)
    return text


def sha(data):
    return hashlib.sha256(data).hexdigest()


def octal(raw):
    raw = raw.split(b"\0", 1)[0].strip() or b"0"
    try:
        return int(raw, 8)
    except ValueError:
        return 0


def pax_headers(data):
    result = {}
    while data:
        match = re.match(rb"(\d+) ", data)
        if not match:
            break
        length = int(match.group(1))
        record = data[len(match.group(0)):length - 1]
        if b"=" in record:
            key, value = record.split(b"=", 1)
            result[key.decode(errors="replace")] = value.decode(errors="replace")
        data = data[length:]
    return result


def parse_tar(data):
    entries = []
    off = 0
    longname = None
    longlink = None
    pax = {}
    while off + 512 <= len(data):
        block = data[off:off + 512]
        off += 512
        if not block.strip(b"\0"):
            break
        name = block[0:100].split(b"\0", 1)[0].decode(errors="replace")
        mode = octal(block[100:108])
        uid = octal(block[108:116])
        gid = octal(block[116:124])
        size = octal(block[124:136])
        typeflag = chr(block[156] or 48)
        link = block[157:257].split(b"\0", 1)[0].decode(errors="replace")
        magic = block[257:263]
        uname = block[265:297].split(b"\0", 1)[0].decode(errors="replace")
        gname = block[297:329].split(b"\0", 1)[0].decode(errors="replace")
        prefix = block[345:500].split(b"\0", 1)[0].decode(errors="replace") if magic.startswith(b"ustar") else ""
        if prefix:
            name = prefix + "/" + name
        payload = data[off:off + size]
        off += (size + 511) & ~511
        if typeflag == "L":
            longname = payload.split(b"\0", 1)[0].decode(errors="replace")
            continue
        if typeflag == "K":
            longlink = payload.split(b"\0", 1)[0].decode(errors="replace")
            continue
        if typeflag in ("x", "g"):
            pax.update(pax_headers(payload))
            if typeflag == "x":
                continue
        if longname is not None:
            name = longname
            longname = None
        if longlink is not None:
            link = longlink
            longlink = None
        if "path" in pax:
            name = pax.pop("path")
        if "linkpath" in pax:
            link = pax.pop("linkpath")
        entries.append({
            "name": name, "type": typeflag, "mode": mode & 0o7777, "uid": uid,
            "gid": gid, "uname": uname, "gname": gname, "link": link,
            "size": size, "sha": sha(payload) if typeflag in ("0", "\0", "7") else "",
        })
        pax = {}
    return entries


def decompress(path):
    data = path.read_bytes()
    name = path.name.lower()
    if name.endswith((".tar.gz", ".tgz", ".sh")):
        return gzip.decompress(data)
    if name.endswith((".tar.bz2", ".tbz2")):
        return bz2.decompress(data)
    if name.endswith((".tar.xz", ".txz", ".tar.lzma")):
        return lzma.decompress(data)
    if name.endswith(".tar.zst"):
        proc = run(["zstd", "-dc", path], timeout=120)
        return proc.stdout if proc.returncode == 0 else b""
    if name.endswith(".tar.z"):
        proc = run(["uncompress", "-c", path], timeout=120)
        return proc.stdout if proc.returncode == 0 else b""
    return data


def tar_signature_gzip(path):
    return parse_tar(gzip.decompress(path.read_bytes()))

def tar_signature(path):
    return parse_tar(decompress(path))


def zip_signature(path):
    result = []
    try:
        with zipfile.ZipFile(path) as zf:
            for info in zf.infolist():
                mode = (info.external_attr >> 16) & 0o7777
                data = zf.read(info.filename) if not info.is_dir() else b""
                kind = "d" if info.is_dir() else ("l" if mode & 0o120000 == 0o120000 else "f")
                result.append({"name": info.filename, "type": kind, "mode": mode, "sha": sha(data), "size": len(data)})
    except Exception as exc:
        return f"zip error: {exc}"
    return result


def parse_ar(data):
    if not data.startswith(b"!<arch>\n"):
        return []
    out = []
    off = 8
    while off + 60 <= len(data):
        head = data[off:off + 60]
        off += 60
        rawname = head[:16].decode(errors="replace")
        try:
            uid = int(head[24:30].decode().strip() or 0)
            gid = int(head[30:36].decode().strip() or 0)
            mode = int(head[36:42].decode().strip() or 0, 8)
            size = int(head[48:58].decode().strip() or 0)
        except ValueError:
            break
        payload = data[off:off + size]
        off += size + (size & 1)
        if rawname.startswith("#1/"):
            n = int(rawname[3:].strip())
            name = payload[:n].decode(errors="replace")
            payload = payload[n:]
        else:
            name = rawname.strip()
        out.append((name, uid, gid, mode, payload))
    return out


def tar_bytes_for_member(name, payload):
    if name == "control.tar.gz":
        return gzip.decompress(payload)
    if name.startswith("data.tar"):
        if name.endswith(".gz"):
            return gzip.decompress(payload)
        if name.endswith(".bz2"):
            return bz2.decompress(payload)
        if name.endswith((".xz", ".lzma")):
            return lzma.decompress(payload)
        if name.endswith(".zst"):
            proc = subprocess.run(["zstd", "-dc"], input=payload, capture_output=True)
            return proc.stdout
        return payload
    return payload


def deb_signature(path):
    result = []
    for name, uid, gid, mode, payload in parse_ar(path.read_bytes()):
        item = {"name": name, "uid": uid, "gid": gid, "mode": mode, "size": len(payload)}
        if name == "debian-binary":
            item["sha"] = sha(payload)
        elif name.endswith((".gz", ".bz2", ".xz", ".lzma", ".zst")) or name == "data.tar":
            item["tar"] = parse_tar(tar_bytes_for_member(name, payload))
        result.append(item)
    return result


def stgz_signature(path):
    data = path.read_bytes()
    pos = data.find(b"\x1f\x8b")
    if pos < 0:
        return "no gzip payload"
    return {"header": data[:pos].decode(errors="replace"), "tar": parse_tar(gzip.decompress(data[pos:]))}


def extract_7z_signature(path, tmp):
    outdir = Path(tmp) / "sevenz"
    outdir.mkdir()
    proc = run(["7z", "x", "-y", f"-o{outdir}", path], timeout=120)
    if proc.returncode != 0:
        return "7z extract failed: " + proc.stdout.decode(errors="replace")
    result = []
    for p in sorted(outdir.rglob("*")):
        rel = p.relative_to(outdir).as_posix() + ("/" if p.is_dir() else "")
        if p.is_symlink():
            result.append({"name": rel, "type": "l", "link": os.readlink(p)})
        elif p.is_dir():
            result.append({"name": rel, "type": "d", "mode": oct(p.stat().st_mode & 0o7777)})
        elif p.is_file():
            data = p.read_bytes()
            result.append({"name": rel, "type": "f", "mode": oct(p.stat().st_mode & 0o7777), "sha": sha(data), "size": len(data)})
    return result


def package_signature(path, tmp):
    name = path.name.lower()
    if name.endswith((".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar.zst", ".tar.z", ".tar")):
        return tar_signature(path)
    if name.endswith(".zip"):
        return zip_signature(path)
    if name.endswith(".7z"):
        return extract_7z_signature(path, tmp)
    if name.endswith(".deb"):
        return deb_signature(path)
    if name.endswith(".sh"):
        return stgz_signature(path)
    if name.endswith((".json", ".txt", ".md5", ".sha1", ".sha224", ".sha256", ".sha384", ".sha512")):
        return norm(path.read_text(errors="replace"), path.parent)
    try:
        return {"bytes-sha256": sha(path.read_bytes()), "size": path.stat().st_size}
    except Exception as exc:
        return str(exc)


def configure_fixture(case, seed):
    src = seed / "src"
    shutil.copytree(case, src, symlinks=True)
    build = seed / "build"
    env = {"PATH": "/usr/bin:/bin:/home/linuxbrew/.linuxbrew/bin", "HOME": str(seed), "LANG": "C", "LC_ALL": "C", "CMAKE_RUST_DATA_ROOT": DATA_ROOT_TMP}
    proc = run([ORACLE_CMAKE, "-S", src, "-B", build, "-G", "Ninja"], cwd=seed, env=env, timeout=300)
    if proc.returncode != 0:
        return False, "oracle configure failed\n" + proc.stdout.decode(errors="replace") + proc.stderr.decode(errors="replace")
    if (case / "build.txt").exists():
        proc = run([ORACLE_CMAKE, "--build", build], cwd=seed, env=env, timeout=600)
        if proc.returncode != 0:
            return False, "oracle build failed\n" + proc.stdout.decode(errors="replace") + proc.stderr.decode(errors="replace")
    return True, ""


def cpack_args(case, build, out):
    args = []
    if (case / "preargs.txt").exists():
        for line in (case / "preargs.txt").read_text().splitlines():
            if line.strip():
                args += shlex.split(line)
    if (case / "source.txt").exists():
        args += ["--config", str(build / "CPackSourceConfig.cmake")]
    gen = (case / "generators.txt").read_text().strip() if (case / "generators.txt").exists() else "TGZ"
    args += ["-G", gen, "-B", str(out)]
    if (case / "args.txt").exists():
        for line in (case / "args.txt").read_text().splitlines():
            if line.strip():
                args += shlex.split(line)
    return args


def run_one(binary, case, build, out):
    env = {"PATH": "/usr/bin:/bin:/home/linuxbrew/.linuxbrew/bin", "HOME": str(out), "LANG": "C", "LC_ALL": "C", "CMAKE_RUST_DATA_ROOT": DATA_ROOT_TMP}
    proc = run([binary, *cpack_args(case, build, out)], cwd=build, env=env, timeout=600)
    artifacts = {}
    sigtmp = tempfile.mkdtemp(prefix="cpd-", dir="/tmp")
    try:
        for path in sorted(out.iterdir() if out.exists() else []):
            if path.is_file() and not path.name.startswith(("stdout", "stderr")):
                artifacts[path.name] = package_signature(path, sigtmp)
    finally:
        shutil.rmtree(sigtmp, ignore_errors=True)
    return {
        "exit": proc.returncode,
        "stdout": norm(proc.stdout.decode(errors="replace"), out),
        "stderr": norm(proc.stderr.decode(errors="replace"), out),
        "names": sorted(artifacts),
        "artifacts": artifacts,
    }


def compare_dicts(a, b, path=""):
    if type(a) != type(b):
        return [f"{path}: type {type(a).__name__} != {type(b).__name__}"]
    if isinstance(a, dict):
        diff = []
        for key in sorted(set(a) | set(b)):
            if key not in a:
                diff.append(f"{path}/{key}: only in candidate")
            elif key not in b:
                diff.append(f"{path}/{key}: only in oracle")
            else:
                diff += compare_dicts(a[key], b[key], f"{path}/{key}")
        return diff
    if isinstance(a, list):
        if len(a) != len(b):
            return [f"{path}: list length {len(a)} != {len(b)}\noracle={a!r}\ncandidate={b!r}"]
        diff = []
        for i, (x, y) in enumerate(zip(a, b)):
            diff += compare_dicts(x, y, f"{path}[{i}]")
        return diff
    if a != b:
        return [f"{path}: {a!r} != {b!r}"]
    return []


def one_case(case, candidate, keep):
    tmp = Path(tempfile.mkdtemp(prefix=f"cpackdiff-{case.name}-", dir=keep or "/tmp"))
    try:
        seed = tmp / "seed"
        seed.mkdir()
        ok, message = configure_fixture(case, seed)
        if not ok:
            return case.name, False, message
        build = seed / "build"
        oracle_out = tmp / "oracle-out"
        cand_out = tmp / "candidate-out"
        oracle_out.mkdir()
        cand_out.mkdir()
        oracle = run_one(ORACLE_CPACK, case, build, oracle_out)
        cand = run_one(candidate, case, build, cand_out)
        diffs = []
        for key in ("exit", "stdout", "stderr", "names"):
            if oracle[key] != cand[key]:
                diffs.append(f"{key}:\n--- oracle ---\n{oracle[key]}\n--- candidate ---\n{cand[key]}")
        if oracle["names"] == cand["names"]:
            for name in oracle["names"]:
                diffs += compare_dicts(oracle["artifacts"][name], cand["artifacts"][name], name)
        if diffs:
            return case.name, False, "\n".join(diffs[:20]) + f"\n(work tree: {tmp})"
        if not keep:
            shutil.rmtree(tmp, ignore_errors=True)
        return case.name, True, ""
    except Exception as exc:
        return case.name, False, f"harness error: {exc}\n(work tree: {tmp})"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cpack")
    parser.add_argument("-j", type=int, default=4)
    parser.add_argument("--keep", type=Path)
    parser.add_argument("-v", action="store_true")
    parser.add_argument("cases", nargs="*")
    args = parser.parse_args()
    root = ROOT / "tests/cpack"
    cases = [root / c for c in args.cases] if args.cases else sorted(p for p in root.iterdir() if p.is_dir())
    if args.keep:
        args.keep.mkdir(parents=True, exist_ok=True)
    failures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.j)) as pool:
        for name, ok, detail in pool.map(lambda c: one_case(c, args.candidate.resolve(), str(args.keep) if args.keep else None), cases):
            print(("PASS " if ok else "FAIL ") + name)
            if not ok:
                failures.append(name)
                print(detail)
    print(f"{len(cases) - len(failures)}/{len(cases)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
