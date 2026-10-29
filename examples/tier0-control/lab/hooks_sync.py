"""Lab-only bridge hooks that SYNCHRONISE with the harness: at the hook point named in $HOOK_AT the bridge writes
$HOOK_DIR/reached-<point> and then waits (up to 120 s) for $HOOK_DIR/release-<point>.  The harness uses the pause to kill
the bridge (R1, R2) or to stop the runtime (R3) at an exact point.  Never shipped; the bridge loads it only with --test-hooks."""
import os
import time


class _Hooks:
    def __init__(self):
        self.point, self.dir = os.environ.get('HOOK_AT'), os.environ.get('HOOK_DIR')

    def _at(self, name):
        if name != self.point or not self.dir:
            return
        with open(os.path.join(self.dir, 'reached-' + name), 'w') as f:
            f.write(repr(time.time()))
        release = os.path.join(self.dir, 'release-' + name)
        end = time.time() + 120
        while time.time() < end and not os.path.exists(release):
            time.sleep(0.01)

    def skip_gate(self, name):
        return False

    def after_gates(self):
        self._at('after_gates')

    def after_intent(self):
        self._at('after_intent')

    def after_http(self):
        self._at('after_http')

    def after_response(self):
        self._at('after_response')

    def after_post_check(self):
        self._at('after_post_check')


HOOKS = _Hooks()
