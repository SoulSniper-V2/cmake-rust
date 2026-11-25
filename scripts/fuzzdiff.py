#!/usr/bin/env python3
"""Differential fuzzer comparing oracle and candidate cmake behavior.

Two lanes:
  script: random valid-ish `cmake -P` programs; compare stdout/stderr/rc bytes.
  genex:  project-mode configures using file(GENERATE) with random genex
          content; compare configure output, exit code and generated files.

Deterministic per --seed and case index.  Failing cases are preserved in
<out>/fails/<lane>-<seed>-<idx>/ along with oracle/candidate output and a
greedily minimized reproducer (min.cmake / min-CMakeLists.txt).

Usage: fuzzdiff.py --candidate <staged bin/cmake> [--seed N] [-j N]
       [--script-count M] [--genex-count G] [--out DIR] [--no-minimize]
"""
import argparse
import concurrent.futures
import difflib
import json
import re
import random
import shutil
import subprocess
import sys
from pathlib import Path

VALIDATION = "/tmp/cmake-rust-validation.hSLgEZHR"
ORACLE = Path(VALIDATION) / "kitware-reference/bin/cmake"

# ---------------------------------------------------------------------------
# adversarial values
# ---------------------------------------------------------------------------

def value_pool(rng):
    long_s = "x" * rng.choice((40, 200, 900))
    return [
        "", "a", "abc", "A B C", " ", "  x  ",
        ";", ";;", "a;;b", ";a;", "a;b;c", "\\\\;", "a\\;b", "\\;",
        "[", "]", "[[", "]]", "[[]]", "[x]", "a[b]c",
        "héllo", "üñïçødé", "αβγ", "日本語", "aé中",
        "0", "00", "007", "-0", "+5", "-5", "0x10", "0X1f", "-0xA",
        "2147483647", "-2147483648", "9223372036854775807",
        "-9223372036854775808", "9223372036854775808", "18446744073709551615",
        long_s,
        "line1\nline2", "trailing\n", "\nleading", "a\rb\r\nc",
        "\t", "tab\there", "quote\"q", "back\\slash", "dollar${x}",
        "percent%", "at@at", "hash#h", "paren(x)", "'single'",
        "a b;c", "x=y", "ON", "OFF", "TRUE", "FALSE", "YES", "NO",
        "1.5", "1e3", ".5", "5.", "1.0.2", "v1.2.3",
    ]

def esc_quote(v):
    return v.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")

def argform(rng, v, prefer_unquoted=False):
    """Render value v as a cmake argument, choosing a random quoting form."""
    style = rng.random()
    if "\n" in v or "\r" in v or (style < 0.12 and not prefer_unquoted):
        # bracket form
        eq = "=" * rng.choice((0, 0, 1, 2))
        close = "]" + eq + "]"
        if close in v:
            eq = "==="
            close = "]===]"
        return "[" + eq + "[" + v + close
    if style < (0.45 if prefer_unquoted else 0.15) and v and " " not in v and ";" not in v and "\t" not in v and '"' not in v and "#" not in v and "(" not in v and ")" not in v:
        return v  # unquoted
    return '"' + esc_quote(v) + '"'

class ScriptGen:
    def __init__(self, rng):
        self.rng = rng
        self.pool = value_pool(rng)
        self.vars = []
        self.counter = 0
        self.lines = []
        self.depth = 0
        self.protected = set()

    def v(self):
        return self.rng.choice(self.pool)

    def av(self):
        return argform(self.rng, self.v())

    def var(self):
        avail = [x for x in self.vars if x not in self.protected]
        if avail and self.rng.random() < 0.7:
            return self.rng.choice(avail)
        self.counter += 1
        name = f"v{self.counter}"
        self.vars.append(name)
        return name

    def emit(self, s):
        self.lines.append("  " * self.depth + s)

    # ----- statement generators -----
    def st_set(self):
        n = self.var()
        count = self.rng.choice((0, 1, 1, 2, 3))
        vals = " ".join(self.av() for _ in range(count))
        self.emit(f"set({n} {vals})".rstrip() + (")" if count == 0 else ""))
        if count == 0:
            self.lines[-1] = f"set({n})"

    def st_unset(self):
        if self.vars:
            self.emit(f"unset({self.rng.choice(self.vars)})")

    def st_string(self):
        out = self.var()
        sub = self.rng.choice((
            "REGEX MATCH", "REGEX MATCHALL", "REGEX REPLACE", "REPLACE",
            "CONCAT", "APPEND", "PREPEND", "TOUPPER", "TOLOWER", "LENGTH",
            "SUBSTRING", "STRIP", "GENEX_STRIP", "CONFIGURE", "COMPARE EQUAL",
            "COMPARE LESS", "COMPARE GREATER", "ASCII", "HEX", "MD5", "SHA256",
            "MAKE_C_IDENTIFIER", "JSON GET", "JSON LENGTH",
            "JSON MEMBER", "JSON TYPE", "JSON REMOVE", "JSON SET",
        ))
        r = self.rng
        a, b, c = self.v(), self.v(), self.v()
        A, B, C = argform(r, a), argform(r, b), argform(r, c)
        if sub.startswith("REGEX MATCH"):
            regex = r.choice(("a+", "[a-z]+", "(a)(b)?", "x*y?", ".", "^a", "z$", "[", "(", "a|b", "\\;", "\\[", "héllo", "^$", "(())", "a{2}", "a{1,3}"))
            inp = r.choice((a, "aaabaa", "", "xyz", "a1b2", "héllo wörld"))
            if sub == "REGEX REPLACE":
                rep = r.choice(("X", "", "\\1", "<\\1-\\2>", "\\\\", ";"))
                self.emit(f"string({sub} {argform(r, regex)} {argform(r, rep)} {out} {argform(r, inp)})")
            else:
                self.emit(f"string({sub} {argform(r, regex)} {out} {argform(r, inp)})")
        elif sub == "REPLACE":
            self.emit(f"string(REPLACE {A} {B} {out} {C})")
        elif sub in ("CONCAT",):
            self.emit(f"string(CONCAT {out} {A} {B} {C})")
        elif sub in ("APPEND", "PREPEND"):
            self.emit(f"string({sub} {out} {A} {B})")
        elif sub in ("TOUPPER", "TOLOWER", "LENGTH", "STRIP", "GENEX_STRIP", "MD5", "SHA256"):
            self.emit(f"string({sub} {A} {out})")
        elif sub == "SUBSTRING":
            self.emit(f"string(SUBSTRING {A} {r.randint(-5, 20)} {r.randint(-5, 20)} {out})")
        elif sub == "CONFIGURE":
            tmpl = r.choice(("x@FOO@y", "${FOO}", "a@b@c", "#cmakedefine X", "@ONLY@", "plain", "\\@escaped"))
            self.emit(f"set(FOO {argform(r, b)})")
            self.emit(f"string(CONFIGURE {argform(r, tmpl)} {out}" + (" @ONLY" if r.random() < 0.5 else "") + ")")
        elif sub.startswith("COMPARE"):
            self.emit(f"string({sub} {A} {B} {out})")
        elif sub == "ASCII":
            nums = " ".join(str(r.choice((0, 9, 10, 13, 32, 65, 97, 126, 127, 128, 195, 233, 255))) for _ in range(r.randint(1, 5)))
            self.emit(f"string(ASCII {nums} {out})")
        elif sub == "HEX":
            self.emit(f"string(HEX {A} {out})")
        elif sub == "MAKE_C_IDENTIFIER":
            self.emit(f"string(MAKE_C_IDENTIFIER {A} {out})")
        elif sub.startswith("JSON"):
            js = r.choice((
                '{"a":1,"b":[1,2,3],"c":{"d":"x"}}',
                '[0,1,"two",null,true,false]',
                '{"k":"v","empty":"","arr":[]}',
                '{}', '[]', '{"nested":{"deep":{"x":5}}}',
                '{"a";bad}', 'notjson', '{"u":"héllo"}',
            ))
            op = sub.split()[-1]
            member = r.choice(("a", "b", "c", "0", "missing", "d"))
            idx = r.choice(("0", "1", "2", "5", "-1"))
            if op == "GET":
                self.emit(f"string(JSON {out} GET {argform(r, js)} {r.choice((member, idx))})")
            elif op == "LENGTH":
                self.emit(f"string(JSON {out} LENGTH {argform(r, js)})")
            elif op == "MEMBER":
                self.emit(f"string(JSON {out} MEMBER {argform(r, js)} {idx})")
            elif op == "TYPE":
                self.emit(f"string(JSON {out} TYPE {argform(r, js)} {r.choice((member, idx))})")
            elif op == "REMOVE":
                self.emit(f"string(JSON {out} REMOVE {argform(r, js)} {r.choice((member, idx))})")
            elif op == "SET":
                self.emit(f"string(JSON {out} SET {argform(r, js)} {r.choice((member, idx))} {argform(r, c)})")
        else:
            return

    def st_list(self):
        if not self.vars:
            self.st_set()
        lst = self.rng.choice(self.vars)
        out = self.var()
        r = self.rng
        sub = r.choice((
            "LENGTH", "GET", "JOIN", "SUBLIST", "FIND", "APPEND", "PREPEND",
            "INSERT", "POP_BACK", "POP_FRONT", "REMOVE_ITEM", "REMOVE_AT",
            "REMOVE_DUPLICATES", "REVERSE", "SORT", "TRANSFORM_UPPER",
            "TRANSFORM_LOWER", "TRANSFORM_STRIP", "TRANSFORM_REPLACE",
            "TRANSFORM_PREPEND", "TRANSFORM_APPEND", "FILTER_INCLUDE",
            "FILTER_EXCLUDE",
        ))
        A = self.av()
        if sub == "LENGTH":
            self.emit(f"list(LENGTH {lst} {out})")
        elif sub == "GET":
            idxs = " ".join(str(r.randint(-3, 5)) for _ in range(r.randint(1, 2)))
            self.emit(f"list(GET {lst} {idxs} {out})")
        elif sub == "JOIN":
            self.emit(f"list(JOIN {lst} {A} {out})")
        elif sub == "SUBLIST":
            self.emit(f"list(SUBLIST {lst} {r.randint(-3, 5)} {r.randint(-3, 6)} {out})")
        elif sub == "FIND":
            self.emit(f"list(FIND {lst} {A} {out})")
        elif sub in ("APPEND", "PREPEND"):
            self.emit(f"list({sub} {lst} {A} {self.av()})")
        elif sub == "INSERT":
            self.emit(f"list(INSERT {lst} {r.randint(-3, 5)} {A})")
        elif sub.startswith("POP"):
            self.emit(f"list({sub} {lst} {out})")
        elif sub == "REMOVE_ITEM":
            self.emit(f"list(REMOVE_ITEM {lst} {A})")
        elif sub == "REMOVE_AT":
            idxs = " ".join(str(r.randint(-3, 5)) for _ in range(r.randint(1, 2)))
            self.emit(f"list(REMOVE_AT {lst} {idxs})")
        elif sub in ("REMOVE_DUPLICATES", "REVERSE"):
            self.emit(f"list({sub} {lst})")
        elif sub == "SORT":
            opts = ""
            if r.random() < 0.4:
                opts += " COMPARE " + r.choice(("STRING", "FILE_BASENAME", "NATURAL"))
            if r.random() < 0.4:
                opts += " CASE " + r.choice(("SENSITIVE", "INSENSITIVE"))
            if r.random() < 0.4:
                opts += " ORDER " + r.choice(("ASCENDING", "DESCENDING"))
            self.emit(f"list(SORT {lst}{opts})")
        elif sub.startswith("TRANSFORM"):
            action = sub.split("_", 1)[1]
            if action in ("UPPER", "LOWER", "STRIP"):
                sel = ""
                if r.random() < 0.3:
                    sel = " " + r.choice(("AT", "FOR")) + " " + str(r.randint(-3, 5)) + (" " + str(r.randint(-3, 5)) if r.random() < 0.5 else "")
                if r.random() < 0.2:
                    sel = " REGEX " + argform(r, r.choice(("a+", "^x", "[", "b$")))
                self.emit(f"list(TRANSFORM {lst} {action}{sel})")
            elif action == "REPLACE":
                self.emit(f"list(TRANSFORM {lst} REPLACE {A} {self.av()})")
            else:
                self.emit(f"list(TRANSFORM {lst} {action} {A})")
        elif sub.startswith("FILTER"):
            mode = sub.split("_")[1]
            self.emit(f"list(FILTER {lst} {mode} REGEX {argform(r, r.choice(('a+', '^x', '[', '.*', 'héllo')))})")

    def st_math(self):
        out = self.var()
        r = self.rng
        def num():
            return r.choice(("0", "1", "7", "-3", "0x10", "0xff", "007", "+5",
                             "2147483647", "-2147483648", "9223372036854775807",
                             "-9223372036854775808", "9223372036854775808",
                             "4294967295", "0xFFFFFFFFFFFFFFFF"))
        def expr(depth=0):
            if depth > 2 or r.random() < 0.4:
                return num()
            op = r.choice(("+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^", "&&", "||", "==", "<", ">", "<=", ">=", "!="))
            l = expr(depth + 1)
            rr = expr(depth + 1)
            s = f"{l} {op} {rr}"
            return f"({s})" if r.random() < 0.6 else s
        e = expr()
        if r.random() < 0.06:
            e = r.choice(("~5", "!0", "!5", "-(3+2)", "+7", "1/0", "x/0", "-", ""))
        self.emit(f"math(EXPR {out} \"{e}\")")

    def cond(self, depth=0):
        r = self.rng
        if depth > 2 or r.random() < 0.5:
            kind = r.choice(("var", "str", "cmp", "vercmp", "match", "defined", "inlist", "patheq", "const", "exist"))
            A = self.av()
            if kind == "var":
                return r.choice(self.vars) if self.vars and r.random() < 0.8 else A
            if kind == "str":
                return f"STREQUAL {A} {self.av()}"
            if kind == "cmp":
                return f"{A} {r.choice(('LESS', 'GREATER', 'EQUAL', 'LESS_EQUAL', 'GREATER_EQUAL'))} {self.av()}"
            if kind == "vercmp":
                return f"VERSION_LESS {argform(r, r.choice(('1.0', '1.0.0', '2.0.1', '1.0-alpha', '0.5', '10.0')))} {argform(r, r.choice(('1.0', '2.0.1', '1.0.0.0', '1.0-alpha.1', '0.5.2')))}"
            if kind == "match":
                return f"{A} MATCHES {argform(r, r.choice(('a+', '^[a-z]', '[', '.*')))}"
            if kind == "defined":
                tgt = r.choice(self.vars) if self.vars and r.random() < 0.8 else "UNDEFVAR"
                d = "DEFINED " + (tgt if r.random() < 0.5 else f"CACHE{{{tgt}}}" if r.random() < 0.3 else f"ENV{{{tgt}}}")
                return d
            if kind == "inlist":
                lst = r.choice(self.vars) if self.vars else "x"
                return f"{A} IN_LIST {lst}"
            if kind == "patheq":
                return f"{A} PATH_EQUAL {self.av()}"
            if kind == "exist":
                return f"EXISTS {argform(r, r.choice(('.', 'nope.txt', './data')))}"
            return r.choice(("TRUE", "FALSE", "ON", "OFF", "YES", "NO", "1", "0", "NOTFOUND", "x-NOTFOUND", "IGNORE"))
        op = r.choice(("AND", "OR", "NOT", "paren"))
        if op == "NOT":
            return f"NOT {self.cond(depth + 1)}"
        if op == "paren":
            return f"( {self.cond(depth + 1)} )"
        return f"{self.cond(depth + 1)} {op} {self.cond(depth + 1)}"

    def st_if(self):
        self.emit(f"if({self.cond()})")
        self.depth += 1
        for _ in range(self.rng.randint(1, 3)):
            self.stmt()
        if self.rng.random() < 0.5:
            self.depth -= 1
            self.emit("else()")
            self.depth += 1
            self.stmt()
        self.depth -= 1
        self.emit("endif()")

    def st_foreach(self):
        r = self.rng
        loopv = f"lv{self.counter + 1}"
        style = r.random()
        if style < 0.4:
            items = " ".join(self.av() for _ in range(r.randint(0, 4)))
            self.emit(f"foreach({loopv} {items})")
        elif style < 0.6 and self.vars:
            self.emit(f"foreach({loopv} IN LISTS {r.choice(self.vars)})")
        elif style < 0.8:
            self.emit(f"foreach({loopv} RANGE {r.randint(-2, 3)} {r.randint(0, 5)} {r.choice((1, 2, 3))})")
        else:
            self.emit(f"foreach({loopv} IN ITEMS {self.av()} {self.av()})")
        self.depth += 1
        n = r.randint(1, 2)
        for i in range(n):
            if i == n - 1 and r.random() < 0.35:
                self.emit(r.choice(("break()", "continue()")))
            else:
                self.stmt()
        self.depth -= 1
        self.emit("endforeach()")

    def st_while(self):
        # bounded loop using a counter to guarantee termination
        self.counter += 1
        idx = f"w{self.counter}"
        self.protected.add(idx)
        limit = self.rng.randint(1, 4)
        self.emit(f"set({idx} 0)")
        self.emit(f"while({idx} LESS {limit})")
        self.depth += 1
        self.emit(f"math(EXPR {idx} \"${{{idx}}} + 1\")")
        self.stmt()
        if self.rng.random() < 0.4:
            self.emit(self.rng.choice(("break()", "continue()")))
        self.depth -= 1
        self.emit("endwhile()")

    def st_function(self):
        self.counter += 1
        fname = f"fn{self.counter}"
        r = self.rng
        ismacro = r.random() < 0.4
        kw = "macro" if ismacro else "function"
        params = ""
        if not ismacro and r.random() < 0.5:
            params = " a b"
        self.emit(f"{kw}({fname}{params})")
        self.depth += 1
        self.emit(f"set(fr ${{ARGV}})")
        self.emit(f"set(fra ${{ARGN}})")
        if not ismacro:
            self.emit(f"set(f0 ${{ARGV0}})")
        self.stmt()
        if r.random() < 0.5:
            self.emit("block()")
            self.depth += 1
            self.emit(f"set(bp {self.av()})")
            if r.random() < 0.6:
                self.emit("return(PROPAGATE bp)")
            else:
                self.emit("return()")
            self.depth -= 1
            self.emit("endblock()")
        self.depth -= 1
        self.emit(f"end{kw}()")
        args = " ".join(self.av() for _ in range(r.randint(0, 3)))
        self.emit(f"{fname}({args})".rstrip() + ("" if args else ""))

    def st_cpa(self):
        # cmake_parse_arguments requires include in script mode
        if "include(CMakeParseArguments)" not in self.lines:
            self.lines.insert(0, "include(CMakeParseArguments)")
        out = self.var()
        keys = " ".join(self.av() for _ in range(self.rng.randint(0, 2)))
        self.emit(f"cmake_parse_arguments(PA {self.av()} {self.av()} {self.av()} {keys})")
        for suffix in ("KEYWORDS_MISSING_VALUES",):
            self.emit(f"set({out}_{suffix} ${{PA_{suffix}}})")

    def st_cmake_path(self):
        out = self.var()
        r = self.rng
        p = r.choice(("/a/b/c.txt", "a/b/../c", "./x", "../y", "/a//b", "C:\\win\\p",
                      "/a/b/", ".", "..", "/", "", "a/b c/d.txt", "/héllo/路径/f",
                      "~/user", "rel/./../x", "//server/share"))
        P = argform(r, p)
        sub = r.choice(("NORMAL_PATH", "NORMALIZE", "GET_FILENAME", "GET_PARENT_PATH",
                        "GET_STEM", "GET_EXTENSION", "REMOVE_FILENAME", "REPLACE_FILENAME",
                        "REMOVE_EXTENSION", "REPLACE_EXTENSION", "IS_RELATIVE", "IS_ABSOLUTE",
                        "HAS_FILENAME", "HAS_STEM", "HAS_EXTENSION", "APPEND", "CONVERT",
                        "RELATIVE_PATH", "ABSOLUTE_PATH", "HASH"))
        varn = self.var() if r.random() < 0.5 else out
        if sub == "NORMAL_PATH":
            self.emit(f"cmake_path(NORMAL_PATH {out} {P})" if r.random() < 0.3 else f"cmake_path(SET {varn} {P})")
            if r.random() < 0.7:
                self.emit(f"cmake_path(SET {varn} NORMALIZE {P})")
        elif sub == "NORMALIZE":
            self.emit(f"cmake_path(SET {varn} NORMALIZE {P})")
        elif sub.startswith("GET") or sub.startswith("HAS") or sub in ("IS_RELATIVE", "IS_ABSOLUTE", "HASH"):
            what = sub.replace("GET_", "").replace("_PATH", "") if sub.startswith("GET") else sub
            self.emit(f"cmake_path({sub if not sub.startswith('GET') else sub} {out} {P})")
            # canonical form is cmake_path(GET <var> <comp> out)
            self.lines[-1] = f"cmake_path(GET {varn} {sub.split('GET_')[-1] if 'GET' in sub else sub} {out})" if False else self.lines[-1]
            self.emit(f"cmake_path(SET {varn} {P})")
            comp = {"GET_FILENAME": "FILENAME", "GET_PARENT_PATH": "PARENT_PATH", "GET_STEM": "STEM", "GET_EXTENSION": "EXTENSION"}.get(sub, None)
            if comp:
                self.emit(f"cmake_path(GET {varn} {comp} {out})")
            elif sub.startswith("HAS"):
                self.emit(f"cmake_path(HAS_{sub[4:]} {varn} {out})")
            elif sub in ("IS_RELATIVE", "IS_ABSOLUTE", "HASH"):
                self.emit(f"cmake_path({sub} {varn} {out})")
        elif sub in ("REMOVE_FILENAME", "REPLACE_FILENAME", "REMOVE_EXTENSION", "REPLACE_EXTENSION"):
            self.emit(f"cmake_path(SET {varn} {P})")
            extra = f" {argform(r, 'new.txt')}" if "REPLACE_FILENAME" == sub else ""
            self.emit(f"cmake_path({sub} {varn}{extra})")
        elif sub == "APPEND":
            self.emit(f"cmake_path(SET {varn} {P})")
            self.emit(f"cmake_path(APPEND {varn} {argform(r, r.choice(('x', '/abs', 'a/b', '..')))} OUTPUT_VARIABLE {out})" if r.random() < 0.4 else f"cmake_path(APPEND {varn} {argform(r, 'x/y')})")
        elif sub == "CONVERT":
            self.emit(f"cmake_path(CONVERT {P} TO_CMAKE_PATH_LIST {out} NORMALIZE)" if r.random() < 0.5 else f"cmake_path(CONVERT {P} TO_NATIVE_PATH_LIST {out})")
        elif sub in ("RELATIVE_PATH", "ABSOLUTE_PATH"):
            base = argform(r, r.choice((".", "/a", "x/y")))
            self.emit(f"cmake_path(SET {varn} {P})")
            self.emit(f"cmake_path({sub} {varn} BASE_DIRECTORY {base} OUTPUT_VARIABLE {out})")

    def st_file(self):
        out = self.var()
        r = self.rng
        sub = r.choice(("READ", "STRINGS", "GLOB", "GLOB_RECURSE", "SIZE", "READ_HEX", "MD5O", "TIMESTAMPF"))
        if sub == "READ":
            limit = f" LIMIT {r.randint(0, 30)}" if r.random() < 0.3 else ""
            self.emit(f"file(READ data/f{r.randint(0, 3)}.txt {out}{limit})")
        elif sub == "READ_HEX":
            self.emit(f"file(READ data/f0.txt {out} HEX)")
        elif sub == "SIZE":
            self.emit(f"file(SIZE data/f{r.randint(0, 3)}.txt {out})")
        elif sub == "MD5O":
            self.emit(f"file(MD5 data/f0.txt {out})")
        elif sub == "TIMESTAMPF":
            self.emit(f"file(TIMESTAMP data/f0.txt {out} \"%Y\" UTC)")
        elif sub in ("GLOB", "GLOB_RECURSE"):
            pat = r.choice(("data/*.txt", "data/f?.txt", "data/*", "*.txt", "data/sub/*.txt", "no/such/*"))
            extra = " FOLLOW_SYMLINKS" if r.random() < 0.2 else ""
            rel = " RELATIVE ${CMAKE_CURRENT_SOURCE_DIR}" if r.random() < 0.3 else ""
            listen = ""
            self.emit(f"file({sub} {out}{listen}{rel} {argform(r, pat)})")
        else:  # STRINGS
            opts = ""
            if r.random() < 0.3:
                opts += f" LIMIT_COUNT {r.randint(1, 4)}"
            if r.random() < 0.3:
                opts += f" LIMIT_INPUT {r.randint(1, 40)}"
            if r.random() < 0.3:
                opts += f" REGEX {argform(r, r.choice(('a+', '^x', '[a-z]')))}"
            self.emit(f"file(STRINGS data/f0.txt {out}{opts})")

    def st_separgs(self):
        out = self.var()
        r = self.rng
        if r.random() < 0.5:
            self.emit(f"separate_arguments({out} NATIVE_COMMAND {self.av()})")
        else:
            self.emit(f"separate_arguments({out} UNIX_COMMAND {self.av()})" if r.random() < 0.5 else f"separate_arguments({out} WINDOWS_COMMAND {self.av()})")

    def st_message(self):
        mode = self.rng.choice(("", "", "STATUS", "STATUS", "NOTICE", "VERBOSE", "DEBUG", "WARNING", "AUTHOR_WARNING", "DEPRECATION", "TRACE"))
        vals = " ".join(self.av() for _ in range(self.rng.randint(1, 3)))
        self.emit(f"message({mode} {vals})".replace("  ", " ", 1) if mode else f"message({vals})")

    def st_expand(self):
        r = self.rng
        out = self.var()
        base = self.var()
        self.emit(f"set({base} {self.av()})")
        kind = r.random()
        if kind < 0.3:
            # nested/indirect expansion
            self.emit(f"set(ptr{self.counter} {base})")
            self.emit(f"message(STATUS \"X=[${{ptr{self.counter}}}] V=[${{${{ptr{self.counter}}}}}]\")")
        elif kind < 0.5:
            self.emit(f"message(STATUS \"E=[$ENV{{HOME}}] NE=[$ENV{{NO_SUCH_ENV_VAR_XYZ}}]\")")
        elif kind < 0.7:
            self.emit(f"set({out} \"pre${{{base}}}post ${{undefined_xyz}} ${{${{base}}}}\")")
        elif kind < 0.85:
            self.emit(f"message(STATUS \"C=[$CACHE{{{base}}}]\")")
        else:
            # escapes in quoted arguments
            esc = r.choice(("\\t", "\\n", "\\r", "\\;", "\\\"", "\\\\", "\\x", "\\ ", "\\$"))
            self.emit(f"set({out} \"a{esc}b\")")

    def st_printvar(self):
        if self.vars:
            n = self.rng.choice(self.vars)
            self.emit(f"message(STATUS \"{n}=[${{{n}}}]\")")

    def stmt(self):
        # recursion guard for block-ish statements
        simple = (self.st_set, self.st_unset, self.st_string, self.st_list, self.st_math,
                  self.st_separgs, self.st_file, self.st_message, self.st_printvar,
                  self.st_cmake_path, self.st_cpa, self.st_expand)
        compound = (self.st_if, self.st_foreach, self.st_while, self.st_function)
        if self.depth >= 2:
            choice = self.rng.choice(simple)
        else:
            choice = self.rng.choice(simple * 3 + compound)
        try:
            choice()
        except RecursionError:
            self.st_set()

    def error_tail(self):
        """One intentional error at end of script for error-text comparison."""
        r = self.rng
        out = self.var()
        kind = r.choice(("regex", "math", "listrange", "json", "cpa", "path",
                         "string-sub", "unknowncmd", "mathdiv", "transform"))
        if kind == "regex":
            self.emit(f"string(REGEX MATCHALL {argform(r, r.choice(('[', '(', 'a{2,1}', '*')))} {out} {self.av()})")
        elif kind == "math":
            self.emit(f"math(EXPR {out} \"{r.choice(('(', '1+', '-', '', '1/0', '3/0', 'x'))}\")")
        elif kind == "listrange":
            lst = r.choice(self.vars) if self.vars else out
            self.emit(f"list(GET {lst} 99 {out})")
        elif kind == "json":
            self.emit(f"string(JSON {out} GET {argform(r, r.choice(('{bad', '[1,', 'x')))} a)")
        elif kind == "cpa":
            self.emit("cmake_parse_arguments()")
        elif kind == "path":
            self.emit(f"cmake_path(GET nosuchvar WRONGCOMP {out})")
        elif kind == "string-sub":
            self.emit(f"string(NOSUCHSUB {out})")
        elif kind == "unknowncmd":
            self.emit("this_command_does_not_exist_xyz()")
        elif kind == "mathdiv":
            self.emit(f"math(EXPR {out} \"1 / 0\")")
        else:
            self.emit(f"list(TRANSFORM nosuchlist NOSUCHACTION)")

    def generate(self):
        n = self.rng.randint(6, 16)
        for _ in range(n):
            self.stmt()
        # print all vars at end for state comparison
        for name in sorted(set(self.vars)):
            self.emit(f"message(STATUS \"F:{name}=[${{{name}}}]\")")
        if self.rng.random() < 0.4:
            self.error_tail()
        return "\n".join(self.lines) + "\n"


# ---------------------------------------------------------------------------
# scratch data tree
# ---------------------------------------------------------------------------

def write_data_tree(d: Path, rng: random.Random):
    data = d / "data"
    (data / "sub").mkdir(parents=True, exist_ok=True)
    files = {
        "f0.txt": b"alpha beta\ngamma;delta\nh\xc3\xa9llo\nline4\nline5\n" + b"x" * 100 + b"\n",
        "f1.txt": b"",
        "f2.txt": b"no newline at end",
        "f3.txt": b"a\x00b\x01c\xffbinary\nclean line\n",
    }
    for name, content in files.items():
        (data / name).write_bytes(content)
    (data / "sub" / "f0.txt").write_bytes(b"nested\n")


# ---------------------------------------------------------------------------
# case execution
# ---------------------------------------------------------------------------

def _no_core():
    import resource
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def run_cmake(cmake: Path, args, cwd: Path, timeout=10):
    try:
        p = subprocess.run([str(cmake), *args], cwd=cwd, capture_output=True, timeout=timeout,
                           preexec_fn=_no_core)
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return -99, b"", b"TIMEOUT"


def compare_case(cmake_o: Path, cmake_c: Path, d: Path, scriptname: str):
    ro = run_cmake(cmake_o, ["-P", scriptname], d)
    rc = run_cmake(cmake_c, ["-P", scriptname], d)
    return ro, rc


def outputs_equal(ro, rc):
    return ro == rc


def diffkey(ro, rc):
    parts = []
    for label, i in (("rc", None), ("stdout", 1), ("stderr", 2)):
        pass
    if ro[0] != rc[0]:
        parts.append(f"rc:{ro[0]}!={rc[0]}")
    for idx, label in ((1, "stdout"), (2, "stderr")):
        if ro[idx] != rc[idx]:
            a = ro[idx].decode(errors="replace").splitlines()
            b = rc[idx].decode(errors="replace").splitlines()
            for x, y in zip(a + [""] * len(b), b + [""] * len(a)):
                if x != y:
                    parts.append(f"{label}:{x[:120]}|||{y[:120]}")
                    break
            else:
                parts.append(f"{label}:len")
            break
    return "|".join(parts) or "unknown"


# ---------------------------------------------------------------------------
# minimizer
# ---------------------------------------------------------------------------

def still_differs(cmake_o, cmake_c, d, scriptname):
    ro, rc = compare_case(cmake_o, cmake_c, d, scriptname)
    return not outputs_equal(ro, rc)


def minimize_script(cmake_o, cmake_c, d: Path, scriptname="script.cmake"):
    text = (d / scriptname).read_bytes()
    try:
        lines = text.decode(errors="surrogateescape").splitlines(keepends=True)
    except Exception:
        return text
    if len(lines) <= 1:
        return text

    def test(ls):
        (d / scriptname).write_bytes("".join(ls).encode(errors="surrogateescape"))
        return still_differs(cmake_o, cmake_c, d, scriptname)

    orig = list(lines)
    (d / scriptname).write_bytes(text)
    if not test(list(lines)):
        (d / scriptname).write_bytes(text)
        return text
    cur = list(lines)
    # greedy chunk removal then line removal
    chunk = max(1, len(cur) // 2)
    while chunk >= 1:
        i = 0
        while i < len(cur):
            trial = cur[:i] + cur[i + chunk:]
            if trial and test(trial):
                cur = trial
            else:
                i += 1
        chunk //= 2
    # final line-by-line sweep
    i = 0
    while i < len(cur):
        trial = cur[:i] + cur[i + 1:]
        if trial and test(trial):
            cur = trial
        else:
            i += 1
    result = "".join(cur).encode(errors="surrogateescape")
    (d / scriptname).write_bytes(orig and text)
    return result


# ---------------------------------------------------------------------------
# script lane worker
# ---------------------------------------------------------------------------

def script_case(task):
    seed, idx, cmake_o, cmake_c, outdir, do_min = task
    rng = random.Random((seed * 1000003 + idx * 7919) ^ 0x5F3759DF)
    gen = ScriptGen(rng)
    text = gen.generate()
    d = Path(outdir) / "tmp" / f"s-{seed}-{idx}"
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True)
    write_data_tree(d, rng)
    (d / "script.cmake").write_bytes(text.encode(errors="surrogateescape"))
    ro, rc = compare_case(Path(cmake_o), Path(cmake_c), d, "script.cmake")
    if not outputs_equal(ro, rc) and (ro[0] == -99) != (rc[0] == -99):
        # Exactly one side timed out.  This is usually core-dump handler
        # latency racing the timeout under parallelism, not a real
        # difference: re-run both serially with a generous timeout.
        ro = run_cmake(Path(cmake_o), ["-P", "script.cmake"], d, timeout=90)
        rc = run_cmake(Path(cmake_c), ["-P", "script.cmake"], d, timeout=90)
    if outputs_equal(ro, rc):
        shutil.rmtree(d, ignore_errors=True)
        return {"idx": idx, "ok": True}
    key = diffkey(ro, rc)
    faildir = Path(outdir) / "fails" / f"script-{seed}-{idx}"
    shutil.rmtree(faildir, ignore_errors=True)
    faildir.mkdir(parents=True)
    (faildir / "script.cmake").write_bytes(text.encode(errors="surrogateescape"))
    write_data_tree(faildir, rng)
    (faildir / "oracle.out").write_bytes(ro[1])
    (faildir / "oracle.err").write_bytes(ro[2])
    (faildir / "candidate.out").write_bytes(rc[1])
    (faildir / "candidate.err").write_bytes(rc[2])
    (faildir / "rc.txt").write_text(f"oracle={ro[0]} candidate={rc[0]}\nkey={key}\n")
    minimized = b""
    if do_min:
        minimized = minimize_script(Path(cmake_o), Path(cmake_c), d, "script.cmake")
        if minimized:
            (faildir / "min.cmake").write_bytes(minimized)
    shutil.rmtree(d, ignore_errors=True)
    return {"idx": idx, "ok": False, "key": key, "dir": str(faildir),
            "min": minimized.decode(errors="replace") if minimized else ""}


# ---------------------------------------------------------------------------
# genex lane
# ---------------------------------------------------------------------------

GENEX_TERMINALS = [
    "x", "", "a;b", "$<CONFIG>", "$<CONFIGURATION>", "${x}",
    "$<TARGET_PROPERTY:tgt,NAME>", "$<TARGET_PROPERTY:tgt,TYPE>",
    "$<TARGET_PROPERTY:missing>", "$<TARGET_NAME_IF_EXISTS:tgt>",
    "$<TARGET_NAME_IF_EXISTS:nope>", "$<TARGET_OBJECTS:objl>",
]

def gen_genex(rng, depth=0):
    if depth > 2 or rng.random() < 0.35:
        return rng.choice(GENEX_TERMINALS + ["plain", "", "héllo", "a b"])
    op = rng.choice(("BOOL", "NOT", "AND", "OR", "IF", "STREQUAL", "EQUAL",
                     "JOIN", "UPPER_CASE", "LOWER_CASE", "IN_LIST", "VERSION_LESS",
                     "LIST:LENGTH", "1", "0", "bad", "SEMICOLON"))
    inner = lambda: gen_genex(rng, depth + 1)
    if op == "BOOL":
        return f"$<BOOL:{inner()}>"
    if op == "NOT":
        return f"$<NOT:{inner()}>"
    if op in ("AND", "OR"):
        return f"$<{op}:{inner()},{inner()}>"
    if op == "IF":
        return f"$<IF:{inner()},{inner()},{inner()}>"
    if op in ("STREQUAL", "EQUAL", "VERSION_LESS"):
        return f"$<{op}:{inner()},{inner()}>"
    if op == "JOIN":
        return f"$<JOIN:{inner()},{rng.choice((',', ';', '', '-'))}>"
    if op in ("UPPER_CASE", "LOWER_CASE"):
        return f"$<{op}:{inner()}>"
    if op == "IN_LIST":
        return f"$<IN_LIST:{inner()},{inner()}>"
    if op == "LIST:LENGTH":
        return f"$<LIST:LENGTH,{inner()}>"
    if op in ("1", "0"):
        return f"$<{op}:{inner()}>"
    if op == "SEMICOLON":
        return f"a$<SEMICOLON>b"
    return f"$<BADGENEX:{inner()}>"


def genex_case(task):
    seed, idx, cmake_o, cmake_c, outdir, do_min = task
    rng = random.Random((seed * 1000033 + idx * 7919) ^ 0x9E3779B9)
    n = rng.randint(1, 4)
    chunks = [gen_genex(rng) for _ in range(n)]
    content = "\n".join(chunks)
    proptext = ""
    for i in range(rng.randint(0, 3)):
        proptext += f"set_property(TARGET tgt PROPERTY P{i} {rng.choice(('v;x', 'a b', '', '$<CONFIG>'))})\n"
    lists = f"set(x {rng.choice(('a;b', '', 'p;q;r'))})\n"
    cmakelists = f"""cmake_minimum_required(VERSION 3.25)
project(gx C)
add_executable(tgt main.c)
add_library(objl OBJECT main.c)
{lists}{proptext}file(GENERATE OUTPUT gen.txt CONTENT {json.dumps(content)[1:-1] if False else ''}"{content.replace(chr(92), chr(92)*2).replace(chr(34), chr(92)+chr(34))}")
file(GENERATE OUTPUT $<CONFIG>-o.txt CONTENT "$<TARGET_FILE:tgt>" )
"""
    d = Path(outdir) / "tmp" / f"g-{seed}-{idx}"
    results = []
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True)
    (d / "CMakeLists.txt").write_text(cmakelists)
    (d / "main.c").write_text("int main(void){return 0;}\n")
    ros = []
    for cmake in (cmake_o, cmake_c):
        b = d / ("bo" if cmake == cmake_o else "bc")
        p = run_cmake(Path(cmake), ["-S", str(d), "-B", str(b), "-G", "Ninja"], d, timeout=60)
        gen = b / "gen.txt"
        content_out = gen.read_bytes() if gen.is_file() else b"<NOFILE>"
        ros.append((p[0], p[1], p[2], content_out))
    ro, rc = ros
    # normalize build dir paths in stderr/stdout
    def norm(bts, tag):
        bts = bts.replace(str(d / tag).encode(), b"<B>").replace(str(d).encode(), b"<S>")
        return re.sub(rb"\(\d+\.\d+s\)", b"(<T>s)", bts)
    ro_n = (ro[0], norm(ro[1], "bo"), norm(ro[2], "bo"), norm(ro[3], "bo"))
    rc_n = (rc[0], norm(rc[1], "bc"), norm(rc[2], "bc"), norm(rc[3], "bc"))
    if ro_n == rc_n:
        shutil.rmtree(d, ignore_errors=True)
        return {"idx": idx, "ok": True}
    key = diffkey((ro_n[0], ro_n[1], ro_n[2]), (rc_n[0], rc_n[1], rc_n[2]))
    if ro_n[3] != rc_n[3]:
        key += "|genfile"
    faildir = Path(outdir) / "fails" / f"genex-{seed}-{idx}"
    shutil.rmtree(faildir, ignore_errors=True)
    faildir.mkdir(parents=True)
    (faildir / "CMakeLists.txt").write_text(cmakelists)
    (faildir / "main.c").write_text("int main(void){return 0;}\n")
    (faildir / "oracle.out").write_bytes(ro_n[1])
    (faildir / "oracle.err").write_bytes(ro_n[2])
    (faildir / "candidate.out").write_bytes(rc_n[1])
    (faildir / "candidate.err").write_bytes(rc_n[2])
    (faildir / "oracle.gen").write_bytes(ro_n[3])
    (faildir / "candidate.gen").write_bytes(rc_n[3])
    (faildir / "rc.txt").write_text(f"oracle={ro_n[0]} candidate={rc_n[0]}\nkey={key}\n")
    shutil.rmtree(d, ignore_errors=True)
    return {"idx": idx, "ok": False, "key": key, "dir": str(faildir), "min": ""}


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--oracle", type=Path, default=ORACLE)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("-j", type=int, default=4)
    ap.add_argument("--script-count", type=int, default=1000)
    ap.add_argument("--genex-count", type=int, default=0)
    ap.add_argument("--out", type=Path, default=Path("/tmp/fuzzdiff-out"))
    ap.add_argument("--no-minimize", action="store_true")
    args = ap.parse_args()
    out = args.out
    shutil.rmtree(out / "tmp", ignore_errors=True)
    (out / "tmp").mkdir(parents=True, exist_ok=True)
    (out / "fails").mkdir(parents=True, exist_ok=True)
    co = str(args.oracle)
    cc = str(args.candidate)
    results = {"script": {"total": 0, "ok": 0}, "genex": {"total": 0, "ok": 0}}
    buckets = {}

    def run_lane(lane, count, worker):
        if count == 0:
            return
        tasks = [(args.seed, i, co, cc, str(out), not args.no_minimize) for i in range(count)]
        done = 0
        with concurrent.futures.ThreadPoolExecutor(args.j) as ex:
            for res in ex.map(worker, tasks, chunksize=4):
                results[lane]["total"] += 1
                done += 1
                if res["ok"]:
                    results[lane]["ok"] += 1
                else:
                    buckets.setdefault(res["key"], []).append(res["dir"])
                    if res.get("min"):
                        (out / "fails" / f"{lane}-{args.seed}-{res['idx']}" / "min.cmake").write_text(res["min"])
                if done % 500 == 0:
                    print(f"{lane}: {done}/{count} mismatches={results[lane]['total'] - results[lane]['ok']}", flush=True)

    run_lane("script", args.script_count, script_case)
    run_lane("genex", args.genex_count, genex_case)
    summary = {
        "seed": args.seed,
        "results": results,
        "buckets": {k: {"count": len(v), "example": v[0]} for k, v in sorted(buckets.items(), key=lambda x: -len(x[1]))},
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    shutil.rmtree(out / "tmp", ignore_errors=True)
    print(json.dumps(summary, indent=2))
    return 0 if results["script"]["total"] == results["script"]["ok"] and results["genex"]["total"] == results["genex"]["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
