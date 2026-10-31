"""Exclusive persistent cache slots shared by independent runner processes."""

from contextlib import contextmanager
import fcntl
import os
import math
from pathlib import Path
import time


@contextmanager
def cache_slot(root: Path, *, slots: int = 3, timeout: float = 90):
    """Holds one persistent cache directory for the complete lifetime of a job plan."""
    if type(slots) is not int or not 1 <= slots <= 128 or not isinstance(timeout,(int,float)) or not math.isfinite(timeout) or timeout < 0:
        raise ValueError("cache slots need a positive slot count and a nonnegative wait")
    root.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + timeout
    while True:
        for number in range(1, slots + 1):
            descriptor = os.open(root / f"slot-{number}.lock", os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW, 0o600)
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                os.close(descriptor)
                continue
            directory = root / f"slot-{number}"
            try:
                directory.mkdir(exist_ok=True)
                yield directory
            finally:
                os.close(descriptor)
            return
        left = deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("no persistent cache slot became available")
        time.sleep(min(.25, left))
