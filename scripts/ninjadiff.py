#!/usr/bin/env python3
"""Structural diff of two build.ninja files (oracle vs candidate).

Parses `build` statements (with their indented variables), keys them by the
first explicit output, and reports statements missing on either side plus
per-field differences, grouped by category so that large generated files
(e.g. LLVM) can be triaged.  Paths are compared after replacing the two build
directories and tool paths given with --replace OLD=NEW.

Usage: scripts/ninjadiff.py ORACLE CANDIDATE [--replace A=B ...] [--show N]
"""
import argparse
import collections
import re
import sys


def parse(path, replacements):
    text = open(path, errors="replace").read()
    for old, new in replacements:
        text = text.replace(old, new)
    statements = {}
    order = []
    current = None
    lines = text.split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]
        # Join ninja line continuations ("$\n").
        while line.endswith("$") and not line.endswith("$$") and i + 1 < len(lines):
            i += 1
            line = line[:-1] + lines[i].lstrip()
        i += 1
        if line.startswith("build "):
            body = line[len("build "):]
            outs, _, rest = split_unescaped(body, ":")
            rule_and_deps = rest.strip().split(" ", 1)
            rule = rule_and_deps[0]
            deps = rule_and_deps[1] if len(rule_and_deps) > 1 else ""
            explicit_outs, _, implicit_outs = outs.partition(" | ")
            explicit, implicit, order_only = split_deps(deps)
            key = explicit_outs.split(" ")[0]
            current = {
                "outputs": explicit_outs,
                "implicit_outs": implicit_outs,
                "rule": rule,
                "explicit": explicit,
                "implicit": implicit,
                "order_only": order_only,
                "vars": {},
            }
            statements[key] = current
            order.append(key)
        elif line.startswith("  ") and current is not None and " = " in line:
            name, _, value = line.strip().partition(" = ")
            current["vars"][name] = value
        elif not line.startswith("  "):
            current = None
    return statements, order


def split_unescaped(text, sep):
    index = 0
    while True:
        index = text.find(sep, index)
        if index < 0:
            return text, "", ""
        if index > 0 and text[index - 1] == "$":
            index += 1
            continue
        return text[:index], sep, text[index + 1:]


def split_deps(deps):
    explicit, _, rest = deps.partition(" || ")
    order_only = rest
    explicit, _, implicit = explicit.partition(" | ")
    return explicit.strip(), implicit.strip(), order_only.strip()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("oracle")
    parser.add_argument("candidate")
    parser.add_argument("--replace", action="append", default=[])
    parser.add_argument("--show", type=int, default=3)
    args = parser.parse_args()
    replacements = [tuple(item.split("=", 1)) for item in args.replace]
    oracle, oracle_order = parse(args.oracle, replacements)
    candidate, _ = parse(args.candidate, replacements)

    missing = [key for key in oracle_order if key not in candidate]
    extra = [key for key in candidate if key not in oracle]
    categories = collections.defaultdict(list)
    for key in oracle_order:
        if key not in candidate:
            continue
        a, b = oracle[key], candidate[key]
        for field in ("outputs", "implicit_outs", "rule", "explicit", "implicit", "order_only"):
            if a[field] != b[field]:
                categories[(a["rule"].split("_")[0] if "_" in a["rule"] else a["rule"], field)].append((key, a[field], b[field]))
        for name in sorted(set(a["vars"]) | set(b["vars"])):
            if a["vars"].get(name) != b["vars"].get(name):
                categories[(a["rule"].split("__")[0], "var:" + name)].append(
                    (key, a["vars"].get(name), b["vars"].get(name)))

    print(f"statements: oracle {len(oracle)}, candidate {len(candidate)}")
    print(f"missing in candidate: {len(missing)}; extra in candidate: {len(extra)}")
    for key in missing[: args.show]:
        print(f"  missing: {key} ({oracle[key]['rule']})")
    rules = collections.Counter(oracle[key]["rule"].split("__")[0] for key in missing)
    for rule, count in rules.most_common(10):
        print(f"    missing by rule {rule}: {count}")
    for key in extra[: args.show]:
        print(f"  extra: {key} ({candidate[key]['rule']})")
    print()
    for (rule, field), items in sorted(categories.items(), key=lambda kv: -len(kv[1])):
        print(f"[{len(items)}] {rule} {field}")
        for key, a, b in items[: args.show]:
            print(f"    {key}")
            print(f"      oracle:    {str(a)[:400]}")
            print(f"      candidate: {str(b)[:400]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
