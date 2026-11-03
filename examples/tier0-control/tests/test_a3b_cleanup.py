"""Real process regression: wrapper exit must not hide a surviving worker."""
import asyncio
from pathlib import Path
import os
import select
import signal
import subprocess
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'lab'))
from cases_a3b import stop_exercise_group


WORKER = """
import os, signal, sys, time
if os.fork() == 0:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    print('ready', flush=True)
    time.sleep(30)
    os._exit(0)
if sys.argv[1] == 'exit':
    os._exit(0)
time.sleep(30)
"""


@unittest.skipUnless(hasattr(os, 'fork') and hasattr(os, 'killpg'), 'POSIX process groups required')
class ExerciseGroupCleanup(unittest.TestCase):
    def spawn(self, code, *args):
        child = subprocess.Popen([sys.executable, '-I', '-B', '-c', code, *args],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
        self.addCleanup(self.release, child)
        return child

    @staticmethod
    def release(child):
        # Also clean up when the regression fails on the old implementation.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=5)
        child.stdout.close()
        child.stderr.close()

    def assert_worker_closed(self, child):
        # The worker inherits stdout. EOF proves it stopped even on systems
        # where an orphan's zombie has not yet been reaped by PID 1.
        ready, _, _ = select.select([child.stdout], [], [], 3)
        self.assertTrue(ready, 'descendant still holds stdout after group cleanup')
        self.assertEqual(child.stdout.read(1), b'')

    def check_surviving_worker(self, wrapper_exits):
        child = self.spawn(WORKER, 'exit' if wrapper_exits else 'wait')
        ready, _, _ = select.select([child.stdout], [], [], 5)
        self.assertTrue(ready, 'worker did not become ready')
        self.assertEqual(child.stdout.readline(), b'ready\n')
        if wrapper_exits:
            self.assertEqual(child.wait(timeout=5), 0)
        else:
            self.assertIsNone(child.poll())
            # Exercise the same finally cleanup after the wrapper wait times out.
            with self.assertRaises(subprocess.TimeoutExpired):
                child.wait(timeout=0.05)
        asyncio.run(stop_exercise_group(child, grace_seconds=0.1))
        self.assertIsNotNone(child.returncode)
        self.assert_worker_closed(child)

    def test_exited_wrapper_does_not_hide_sigterm_ignoring_worker(self):
        self.check_surviving_worker(wrapper_exits=True)

    def test_timeout_cleanup_kills_sigterm_ignoring_worker(self):
        self.check_surviving_worker(wrapper_exits=False)

    def test_already_exited_group_keeps_success_exit_code(self):
        child = self.spawn('pass')
        self.assertEqual(child.wait(timeout=5), 0)
        asyncio.run(stop_exercise_group(child, grace_seconds=0.1))
        self.assertEqual(child.returncode, 0)
        self.assert_worker_closed(child)


if __name__ == '__main__':
    unittest.main()
