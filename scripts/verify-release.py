#!/usr/bin/env python3
"""Compatibility entry point for the full upstream conformance harness."""

from pathlib import Path
import runpy

runpy.run_path(str(Path(__file__).with_name("conformance") / "run.py"), run_name="__main__")
