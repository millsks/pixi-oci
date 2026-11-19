"""The smallest application: one file, added with --add hello.py:/app/."""

import platform
import sys

print(f"Hello from python {platform.python_version()} at {sys.prefix}")
