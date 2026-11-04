"""Lab-only bridge hooks for the soak (lab/soak.py): a hook point pauses only when the soak has ARMED it.  At a point the bridge
removes $HOOK_DIR/arm-<point>; if it was there, the bridge writes reached-<point> and waits (up to 30 s) for release-<point>.  Every
other request passes straight through, so one armed request at a time gets an exact crash or stop point.  Never shipped; the bridge
loads it only with --test-hooks."""
import os
import time


class _Hooks:
    def __init__(self):
        self.dir = os.environ.get('HOOK_DIR')

    def _at(self, name):
        if not self.dir:
            return
        try:
            os.remove(os.path.join(self.dir, 'arm-' + name))
        except FileNotFoundError:
            return
        with open(os.path.join(self.dir, 'reached-' + name), 'w') as f:
            f.write(repr(time.time()))
        release = os.path.join(self.dir, 'release-' + name)
        end = time.time() + 30
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
