"""The fake transport must refuse unexpected destinations or shell syntax."""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'lab'))
from fake_ssh_a3b import validate

PREFIX = ['-p', '2222', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=5',
          '-o', 'StrictHostKeyChecking=accept-new', 'a3b@must-not-resolve.invalid']
COMMAND = ("curl --silent --max-time 4 -w '\\n%{http_code}' -X POST "
           "-H 'Content-Type: application/json' -H 'x-ia2-origin: tier0.lab.test' "
           "-d '{\"name\":\"sp_flow\",\"value\":1065353216}' http://127.0.0.1:24502/write")


class FakeSshBoundary(unittest.TestCase):
    def test_original_text_and_json_are_preserved(self):
        command, tokens = validate(PREFIX + [COMMAND], 24502)
        self.assertEqual(command, COMMAND)
        self.assertEqual(json.loads(tokens[tokens.index('-d') + 1]), {'name': 'sp_flow', 'value': 1065353216})

    def test_absent_label_is_valid(self):
        validate(PREFIX + [COMMAND.replace("-H 'x-ia2-origin: tier0.lab.test' ", '')], 24502)

    def test_wrong_ssh_target_and_options_are_refused(self):
        for argv in [PREFIX[:-1] + ['other.invalid', COMMAND], PREFIX + ['-L', COMMAND], ['-p', '22'] + PREFIX[2:] + [COMMAND], []]:
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                validate(argv, 24502)

    def test_other_commands_destinations_and_unquoted_expansion_are_refused(self):
        for command in [COMMAND + '; true', COMMAND + ' | cat', COMMAND + '\ntrue', 'systemctl status ia2-runtime',
                        COMMAND.replace(':24502/', ':24503/'), COMMAND.replace('127.0.0.1', 'example.invalid'),
                        COMMAND.replace('/write', '/force'), COMMAND.replace("'x-ia2-origin: tier0.lab.test'", '"x-ia2-origin: $(true)"'),
                        COMMAND.replace('1065353216', '$(true)'), COMMAND.replace('sp_flow', 'unexpected')]:
            with self.subTest(command=command), self.assertRaises(ValueError):
                validate(PREFIX + [command], 24502)


if __name__ == '__main__':
    unittest.main()
