"""Use the platform's strongest sync. If F_FULLFSYNC exists, failure is fatal, even
when a filesystem does not support it: silently using weaker fsync would lose the
promised durability. Plain fsync is used only on platforms without F_FULLFSYNC."""
import os

try:
    import fcntl
except ImportError:                                  # not on this platform
    fcntl = None


def sync(fd):
    full = getattr(fcntl, 'F_FULLFSYNC', None) if fcntl is not None else None
    if full is not None:
        fcntl.fcntl(fd, full)
    else:
        os.fsync(fd)
