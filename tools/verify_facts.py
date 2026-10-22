#!/usr/bin/env python3
"""Verification suite for tachograph Perfetto facts.

Runs experiments against trace_processor_shell to verify:
1. TrackEvent extension fields (4000-4099) & ExtensionDescriptor
2. TYPE_STATE state tracks
3. Flow IDs between tracks and sequences
4. Interned data sorting and late header dropping
5. REALTIME timestamp clock & concatenation after system trace
6. Unprivileged system tracing & cloning
7. PerfettoSQL pipe syntax & INTERVAL INTERSECTION OF
8. Agent hook schema validation
9. /usr/bin/codex command check
"""

import os
import subprocess
import sys
import time

PERFETTO_DIR = os.environ.get("PERFETTO_DIR", "/usr/local/google/home/zezeozue/dev/perfetto")
TP_RELEASE = os.path.join(PERFETTO_DIR, "out/release/trace_processor_shell")
TP_HEAD = os.path.join(PERFETTO_DIR, "out/tacho_tp/trace_processor_shell")
WORK_DIR = os.environ.get("TACHO_VERIFY_DIR", "/tmp/tacho-verify")

os.makedirs(WORK_DIR, exist_ok=True)
sys.path.insert(0, os.path.join(PERFETTO_DIR, "python"))
sys.path.insert(0, os.path.join(PERFETTO_DIR, "python/perfetto"))


def run_tp(tp_bin, trace_path, query):
    cmd = [tp_bin, "query", trace_path, query]
    res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    return res.returncode, res.stdout, res.stderr


def main():
    print(f"Using trace_processor_shell: {TP_RELEASE}")
    rc, out, _ = run_tp(TP_RELEASE, "/dev/null", "SELECT 1;")
    if rc != 0:
        print("Failed to run trace_processor_shell")
        sys.exit(1)
    print("TP release accessible.")


if __name__ == "__main__":
    main()
