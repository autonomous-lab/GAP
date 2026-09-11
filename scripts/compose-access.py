#!/usr/bin/env python3
"""Compatibility entrypoint; use microvm-access.py for microVM permissions."""
from pathlib import Path
import runpy

_api = runpy.run_path(str(Path(__file__).with_name('microvm-access.py')))
change, main = _api['change'], _api['main']

if __name__ == '__main__':
    main()
