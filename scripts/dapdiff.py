#!/usr/bin/env python3
"""Differential DAP client for `cmake --debugger`.

Drives the oracle and the candidate through identical scripted Debug
Adapter Protocol sessions over `--debugger-pipe` and diffs the normalized
transcripts.  Standard library only.

Usage: dapdiff.py [--oracle CMAKE] [--candidate CMAKE] [-v] [scenario...]

Normalization: `seq`/`request_seq` are dropped (cppdap allocates them from
two threads, so their interleaving is not deterministic); the scratch
directory and the cmake binaries/roots are replaced by placeholders; and the
messages received while waiting for one step are compared as a sorted set,
since an event and a response sent from different threads may arrive in
either order.  Everything else (bodies, ids, variable trees, ordering of
array elements) must match exactly.
"""

import argparse
import difflib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TIMEOUT = 20.0

MAIN = """\
cmake_minimum_required(VERSION 3.20)
project(DapTest NONE)

function(myfunc arg)
  set(IN_FUNC "f-${arg}")
  message(STATUS "inside func ${IN_FUNC}")
endfunction()

set(TOP_VAR "top value")
set(LIST_VAR a b c)
set(CACHE_VAR "cached" CACHE STRING "doc string")
myfunc(one)
include(${CMAKE_CURRENT_SOURCE_DIR}/sub.cmake)
add_custom_target(tgt COMMAND echo hi)
set_property(TARGET tgt PROPERTY LABELS x y)
enable_testing()
add_test(NAME t1 COMMAND echo t1)
message(STATUS "end ${TOP_VAR}")
"""

SUB = """\
set(SUB_VAR "from sub")
if(SUB_VAR)
  message(STATUS "sub ${SUB_VAR}")
endif()
"""

EXCEPTION_MAIN = """\
cmake_minimum_required(VERSION 3.20)
project(DapExc NONE)
message(WARNING "a plain warning")
message(AUTHOR_WARNING "a dev warning")
message(STATUS "after warnings")
message(SEND_ERROR "a send error")
message(STATUS "after error")
"""

SCRIPT = """\
set(S 1)
message(STATUS "script ${S}")
set(S 2)
"""


class Dap:
    def __init__(self, path, proc):
        self.buf = b""
        self.seq = 0
        self.conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        deadline = time.time() + TIMEOUT
        while True:
            try:
                self.conn.connect(path)
                break
            except OSError:
                if proc.poll() is not None or time.time() > deadline:
                    raise RuntimeError("could not connect to the debugger pipe")
                time.sleep(0.02)
        self.conn.settimeout(TIMEOUT)

    def send(self, command, arguments=None):
        self.seq += 1
        req = {"seq": self.seq, "type": "request", "command": command}
        if arguments is not None:
            req["arguments"] = arguments
        body = json.dumps(req).encode()
        self.conn.sendall(b"Content-Length: %d\r\n\r\n" % len(body) + body)

    def recv(self, timeout=None):
        self.conn.settimeout(TIMEOUT if timeout is None else timeout)
        while True:
            i = self.buf.find(b"\r\n\r\n")
            if i >= 0:
                head = self.buf[:i].decode(errors="replace")
                n = None
                for line in head.split("\r\n"):
                    if line.lower().startswith("content-length:"):
                        n = int(line.split(":", 1)[1])
                if n is None:
                    raise RuntimeError(f"bad DAP header {head!r}")
                if len(self.buf) >= i + 4 + n:
                    msg = json.loads(self.buf[i + 4 : i + 4 + n])
                    self.buf = self.buf[i + 4 + n :]
                    return msg
            chunk = self.conn.recv(65536)
            if not chunk:
                raise EOFError
            self.buf += chunk


class Session:
    """Runs one scripted session, recording normalized step transcripts."""

    def __init__(self, dap, norm):
        self.dap = dap
        self.norm = norm
        self.steps = []

    def _wait(self, title, preds):
        """Read until every predicate in `preds` matched one message."""
        got = []
        pending = list(preds)
        while pending:
            msg = self.dap.recv()
            got.append(msg)
            for p in pending:
                if p(msg):
                    pending.remove(p)
                    break
        self.steps.append((title, sorted(self.norm(m) for m in got)))
        return got

    def request(self, command, arguments=None, events=()):
        self.dap.send(command, arguments)
        preds = [lambda m, c=command: m.get("type") == "response" and m.get("command") == c]
        preds += [lambda m, e=e: m.get("type") == "event" and m.get("event") == e for e in events]
        title = f"{command} {json.dumps(arguments, sort_keys=True) if arguments else ''}"
        got = self._wait(title.strip(), preds)
        for m in got:
            if m.get("type") == "response" and m.get("command") == command:
                return m
        raise AssertionError("unreachable")

    def drain(self, title="<end>"):
        """Collect everything until the debugger closes or goes quiet."""
        got = []
        try:
            while True:
                got.append(self.dap.recv(timeout=3.0))
        except (EOFError, socket.timeout, OSError):
            pass
        self.steps.append((title, sorted(self.norm(m) for m in got)))

    def inspect(self, depth=2):
        """threads, stackTrace, then scopes/variables of the top frame."""
        tid = self.request("threads")["body"]["threads"][0]["id"]
        st = self.request("stackTrace", {"threadId": tid})
        frames = st["body"]["stackFrames"]
        self.frame = frames[0]["id"] if frames else None
        if not frames:
            return tid
        scopes = self.request("scopes", {"frameId": frames[0]["id"]})
        for scope in scopes["body"]["scopes"]:
            self.variables(scope["variablesReference"], depth)
        return tid

    def variables(self, ref, depth, only=None):
        """Expand `ref`; at the first level, only children named in `only`."""
        if not ref or depth <= 0:
            return
        resp = self.request("variables", {"variablesReference": ref})
        for var in resp["body"].get("variables", []):
            if only is not None and var["name"] not in only:
                continue
            # Expanding every cache entry and target property would make the
            # transcript huge; descend one level below scope roots only for
            # small containers.
            child = var.get("variablesReference", 0)
            if child and depth > 1:
                self.variables(child, depth - 1)


def handshake(s, init_args=None, exc_filters=None, breakpoints=()):
    s.request("initialize", init_args or {"adapterID": "dapdiff", "supportsVariableType": True},
              events=("initialized",))
    for path, lines in breakpoints:
        s.request("setBreakpoints", {"source": {"path": path},
                                     "breakpoints": [{"line": ln} for ln in lines]})
    if exc_filters is not None:
        s.request("setExceptionBreakpoints", {"filters": exc_filters})


def scenario_basic(s, src):
    main = os.path.join(src, "CMakeLists.txt")
    sub = os.path.join(src, "sub.cmake")
    handshake(s, breakpoints=[(main, [5, 11, 13, 50]), (sub, [3])])
    s.request("configurationDone", events=("stopped",))
    tid = s.inspect()
    # (upstream dereferences the frame unconditionally: an evaluate without a
    # valid frameId crashes the reference, so always pass the top frame.)
    s.request("evaluate", {"expression": "${TOP_VAR}", "frameId": s.frame, "context": "watch"})
    s.request("evaluate", {"expression": "TOP_VAR", "frameId": s.frame, "context": "hover"})
    s.request("evaluate", {"expression": "${UNDEFINED_THING}x", "frameId": s.frame})
    s.request("next", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("stepIn", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("stepOut", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("continue", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("continue", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("continue", {"threadId": tid})
    s.drain()
    s.request("disconnect", {})


def scenario_exception(s, src):
    handshake(s, exc_filters=["WARNING", "AUTHOR_WARNING", "FATAL_ERROR"])
    s.request("configurationDone", events=("stopped",))
    tid = 1
    for _ in range(3):
        s.request("exceptionInfo", {"threadId": tid})
        s.inspect(depth=1)
        s.request("continue", {"threadId": tid}, events=("stopped",))
    s.request("exceptionInfo", {"threadId": tid})
    s.request("continue", {"threadId": tid})
    s.drain()
    s.request("disconnect", {})


def scenario_default_filters(s, src):
    handshake(s)
    s.request("configurationDone")
    s.drain()
    s.request("disconnect", {})


TARGETS_MAIN = MAIN + """\
add_library(lib STATIC IMPORTED)
add_executable(exe IMPORTED GLOBAL)
add_library(iface INTERFACE)
target_include_directories(iface INTERFACE /inc1 /inc2)
target_compile_definitions(iface INTERFACE D1 D2=2)
target_compile_options(iface INTERFACE -O1)
target_link_options(iface INTERFACE -Wl,-x)
target_link_directories(iface INTERFACE /libdir)
target_compile_features(iface INTERFACE c_std_99)
target_sources(iface INTERFACE FILE_SET HEADERS BASE_DIRS ${CMAKE_CURRENT_SOURCE_DIR} FILES sub.cmake)
target_link_libraries(iface INTERFACE lib)
set_property(TEST t1 PROPERTY LABELS l1 l2)
add_test(t2 echo old style)
message(STATUS "last")
"""


def scenario_targets(s, src):
    main = os.path.join(src, "CMakeLists.txt")
    last = TARGETS_MAIN.count("\n")
    handshake(s, breakpoints=[(main, [last])])
    s.request("configurationDone", events=("stopped",))
    tid = s.request("threads")["body"]["threads"][0]["id"]
    frame = s.request("stackTrace", {"threadId": tid})["body"]["stackFrames"][0]["id"]
    scope = s.request("scopes", {"frameId": frame})["body"]["scopes"][0]
    s.variables(scope["variablesReference"], 8, only=("Targets", "Tests"))
    s.request("continue", {"threadId": tid})
    s.drain()
    s.request("disconnect", {})


def scenario_script(s, src):
    script = os.path.join(src, "script.cmake")
    handshake(s, breakpoints=[(script, [2])])
    s.request("configurationDone", events=("stopped",))
    tid = s.inspect(depth=1)
    s.request("next", {"threadId": tid}, events=("stopped",))
    s.inspect(depth=1)
    s.request("continue", {"threadId": tid})
    s.drain()
    s.request("disconnect", {})


def scenario_disconnect(s, src):
    main = os.path.join(src, "CMakeLists.txt")
    handshake(s, breakpoints=[(main, [9])])
    s.request("configurationDone", events=("stopped",))
    s.request("disconnect", {})
    s.drain()


SCENARIOS = {
    "basic": (MAIN, scenario_basic, False, False),
    "exception": (EXCEPTION_MAIN, scenario_exception, False, False),
    "default-filters": (EXCEPTION_MAIN, scenario_default_filters, False, False),
    "script": (MAIN, scenario_script, True, False),
    "targets": (TARGETS_MAIN, scenario_targets, False, False),
    "disconnect": (MAIN, scenario_disconnect, False, False),
    "daplog": (MAIN, scenario_default_filters, False, True),
}


def cmake_root_of(cmake):
    """CMAKE_ROOT as the binary itself reports it (oracle: its source tree;
    candidate: the staged share/cmake-4.1)."""
    with tempfile.TemporaryDirectory() as d:
        script = Path(d, "root.cmake")
        script.write_text('message("${CMAKE_ROOT}")\n')
        r = subprocess.run([cmake, "-P", str(script)], capture_output=True, text=True)
    return r.stderr.strip()


def run(cmake, name):
    main, driver, script_mode, daplog = SCENARIOS[name]
    work = Path(tempfile.mkdtemp(prefix="dapdiff-"))
    try:
        src = work / "src"
        src.mkdir()
        (src / "CMakeLists.txt").write_text(main)
        (src / "sub.cmake").write_text(SUB)
        (src / "script.cmake").write_text(SCRIPT)
        sock = str(work / "sock")
        log = str(work / "dap.log")
        if script_mode:
            argv = [cmake, "--debugger", "--debugger-pipe", sock, "-P", str(src / "script.cmake")]
        else:
            argv = [cmake, "-S", str(src), "-B", str(work / "build"), "-G", "Ninja",
                    "--debugger", "--debugger-pipe", sock]
        if daplog:
            argv += ["--debugger-dap-log", log]
        proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=work)
        cmake_real = os.path.realpath(cmake)
        cmake_root = cmake_root_of(cmake)
        reps = [(str(work), "<W>"), (os.path.realpath(str(work)), "<W>"),
                (cmake_real, "<CMAKE>"), (cmake, "<CMAKE>"),
                (str(Path(cmake_real).parent), "<BIN>"), (str(Path(cmake).parent), "<BIN>"),
                (cmake_root, "<ROOT>"),
                # CMakeSystemSpecificInformation's _CMAKE_INSTALL_DIR.
                (os.path.dirname(os.path.dirname(cmake_root)), "<INSTALL>")]
        reps.sort(key=lambda r: -len(r[0]))

        def text(s):
            for old, new in reps:
                s = s.replace(old, new)
            # cmMakefile::GetDirectoryId() is the instance pointer in "(%p)"
            # form: address-space dependent on both sides.
            return re.sub(r"\(0x[0-9a-f]+\)", "(<ptr>)", s)

        def norm(msg):
            msg = {k: v for k, v in msg.items() if k not in ("seq", "request_seq")}
            return text(json.dumps(msg, sort_keys=True))

        out = []
        try:
            dap = Dap(sock, proc)
            sess = Session(dap, norm)
            try:
                driver(sess, str(src))
            except Exception as e:  # recorded as a transcript difference
                sess.steps.append(("<harness error>", [f"{type(e).__name__}: {e}"]))
            finally:
                dap.conn.close()
            for title, msgs in sess.steps:
                out.append(f"## {text(title)}")
                out += msgs
        except Exception as e:
            out.append(f"<connect error> {e}")
        try:
            so, se = proc.communicate(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            proc.kill()
            so, se = proc.communicate()
            out.append("<cmake did not exit>")
        out.append(f"## exit {proc.returncode}")
        out.append("## stdout")
        out += text(so.decode(errors="replace")).splitlines()
        out.append("## stderr")
        out += text(se.decode(errors="replace")).splitlines()
        if daplog:
            out.append("## dap log (sorted messages)")
            if os.path.exists(log):
                lines = []
                for raw in Path(log).read_text().splitlines():
                    s = raw.strip()
                    if s.startswith("{"):
                        try:
                            lines.append(norm(json.loads(s)))
                        except ValueError:
                            lines.append(text(s))
                    elif s:
                        lines.append(text(s))
                out += sorted(lines)
            else:
                out.append("<missing>")
        return out
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", default=os.environ.get(
        "UPSTREAM_CMAKE", "/tmp/cmake-rust-validation.hSLgEZHR/kitware-reference/bin/cmake"))
    ap.add_argument("--candidate", required=True,
                    help="staged candidate cmake (needs its share/cmake-4.1 next to bin/)")
    ap.add_argument("-v", action="store_true")
    ap.add_argument("--dump", help="write both transcripts of failing scenarios here")
    ap.add_argument("scenarios", nargs="*")
    args = ap.parse_args()
    names = args.scenarios or list(SCENARIOS)
    failures = 0
    for name in names:
        o = run(args.oracle, name)
        c = run(args.candidate, name)
        if o == c:
            print(f"PASS {name}")
            if args.v:
                print("\n".join(o))
        else:
            failures += 1
            print(f"FAIL {name}")
            if args.dump:
                Path(args.dump).mkdir(parents=True, exist_ok=True)
                Path(args.dump, f"{name}.oracle").write_text("\n".join(o) + "\n")
                Path(args.dump, f"{name}.candidate").write_text("\n".join(c) + "\n")
            sys.stdout.writelines(difflib.unified_diff(
                [x + "\n" for x in o], [x + "\n" for x in c], "oracle", "candidate", n=2))
    print(f"{len(names) - failures}/{len(names)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
