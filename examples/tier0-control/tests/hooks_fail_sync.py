"""Lab-only hooks for the P1 storage-failure process test (loaded with --test-hooks; never part of a production package).

Once the answer to the first POST has been received, every sync of the journal fails with EIO: a disk that went away.  The journal record of
the answer is then the first one that cannot be made durable."""
import errno

from bridge import durable


class _Hooks:
    def after_http(self):
        def bad(fd):
            raise OSError(errno.EIO, 'injected: the disk went away')
        durable.sync = bad


HOOKS = _Hooks()
