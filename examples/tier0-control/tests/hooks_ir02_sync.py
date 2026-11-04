"""Lab-only system-call failure injection, selected by the process test environment."""
import errno
import os
from bridge import durable

STAGE = os.environ['IR02_FAIL_AT']
LAYER = os.environ.get('IR02_SYNC_CALL', 'full')
if LAYER == 'plain':
    durable.fcntl = None  # simulate a platform without F_FULLFSYNC; startup fsync still works


def arm():
    if LAYER == 'plain':
        def bad(fd):
            raise OSError(errno.EIO, 'IR02 injected ordinary fsync EIO')
        durable.os.fsync = bad
    else:
        original = durable.fcntl.fcntl
        full = durable.fcntl.F_FULLFSYNC
        def bad(fd, operation, *args):
            if operation == full:
                raise OSError(errno.EIO, 'IR02 injected F_FULLFSYNC EIO')
            return original(fd, operation, *args)
        durable.fcntl.fcntl = bad


class Hooks:
    def after_gates(self):
        if STAGE == 'intent': arm()
    def after_http(self):
        if STAGE == 'response': arm()
    def after_response(self):
        if STAGE == 'post_check': arm()
    def after_post_check(self):
        if STAGE == 'terminal': arm()

HOOKS = Hooks()
