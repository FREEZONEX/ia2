"""Lab only: block action-result publication while a fixture marker exists."""
import errno
import os
from bridge.trace import TraceWriter

MARKER = os.environ['IR03_TRACE_BLOCK_FILE']
installed = False


class Hooks:
    def after_post_check(self):
        global installed
        if installed:
            return
        installed = True
        original = TraceWriter.emit
        def blocked(writer, event, anchor=None):
            if event.get('kind') == 'action_status' and os.path.exists(MARKER):
                raise OSError(errno.ENOSPC, 'IR03 fixture: trace temporarily unavailable')
            return original(writer, event, anchor)
        TraceWriter.emit = blocked


HOOKS = Hooks()
