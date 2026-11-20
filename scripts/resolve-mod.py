#!/usr/bin/env python3
"""Resolve merge conflicts in registry files by taking the union of lines
from both sides (order: ours then theirs' new lines)."""
import re, sys
from pathlib import Path
for name in sys.argv[1:]:
    p = Path(name); text = p.read_text()
    out = []
    pattern = re.compile(r"<<<<<<< [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n", re.S)
    def repl(m):
        ours = m.group(1).splitlines(keepends=True)
        theirs = m.group(2).splitlines(keepends=True)
        merged = list(ours) + [l for l in theirs if l not in ours]
        return "".join(merged)
    p.write_text(pattern.sub(repl, text))
