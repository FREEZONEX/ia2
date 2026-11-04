"""Test-only reproduction of pre-IR03 publication semantics.

The old engine emitted directly after recording a terminal, so a trace failure
escaped into safe_handle_line and quarantined the request. It had no periodic
publication retry. This minimal fixture recreates that journal transition without
requiring a developer's archived checkout. Local acceptance also tested the actual
archived pre-IR03 engine and process; this fixture is not that full legacy source.
"""
import sys
from pathlib import Path
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import bridge.engine


class LegacyEngine(bridge.engine.Engine):
    def _publish_terminal(self, aid, event=None):
        return self._emit(event if event is not None else self.journal.final_event(aid))

    def retry_terminal_events(self, limit=100):
        pass


if __name__ == '__main__':
    bridge.engine.Engine = LegacyEngine
    from bridge.main import main
    sys.exit(main())
