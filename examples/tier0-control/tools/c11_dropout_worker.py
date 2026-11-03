"""Fault worker for check_c11_regressions.py; always run inside cs agent run."""
import json
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'lab'))
import cases_c11


class DropoutClient(cases_c11.MiniMqttClient):
    def publish(self, topic, payload, *args, **kwargs):
        if topic.endswith('/write') and json.loads(payload).get('value') == 25.0:
            directory = Path(sys.argv[2])
            (directory / 'drop-request').write_text('cut runtime MQTT before publish\n')
            end = time.monotonic() + 5
            while not (directory / 'drop-done.json').exists():
                if time.monotonic() > end:
                    raise TimeoutError('fault proxy did not confirm disconnection')
                time.sleep(0.01)
        return super().publish(topic, payload, *args, **kwargs)


if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] != '--worker':
        raise SystemExit('use check_c11_regressions.py')
    cases_c11.MiniMqttClient = DropoutClient
    raise SystemExit(cases_c11.exercise(str(Path(sys.argv[2]).resolve())))
