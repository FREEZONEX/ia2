"""Fail-closed A3b SSH substitute. Only the IDE child sees this on PATH.

Validate the shell command's *entire* grammar, then run the original text
with /bin/sh and an absolute curl path. Never reconstruct its headers/body.
All shell metacharacters must be quoted inert data; no shell expansion,
pipelines, substitutions or additional commands are accepted.
"""
import datetime
import json
import os
import re
import shlex
import subprocess
import sys


def validate(argv, port):
    prefix = ['-p', '2222', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=5',
              '-o', 'StrictHostKeyChecking=accept-new', 'a3b@must-not-resolve.invalid']
    if argv[:-1] != prefix:
        raise ValueError('unexpected SSH options/target')
    command = argv[-1]
    # Exact generated grammar, including POSIX single-quote escaping in JSON.
    quoted = r"'(?:[^']|'\\'')*'"
    pattern = (r"curl --silent --max-time 4 -w '\\n%\{http_code\}' -X POST "
               r"-H 'Content-Type: application/json' "
               r"(?:-H 'x-ia2-origin: [A-Za-z0-9._-]{1,64}' )?"
               r"-d " + quoted + r" http://127\.0\.0\.1:" + str(port) + r"/write")
    if not re.fullmatch(pattern, command):
        raise ValueError('unexpected remote command (only this run POST /write is allowed)')
    tokens = shlex.split(command)
    body = json.loads(tokens[tokens.index('-d') + 1])
    if set(body) != {'name', 'value'} or body['name'] not in ('sp_flow', 'big', 'unlisted_sp') or type(body['value']) is not int:
        raise ValueError('unexpected write body')
    return command, tokens


def main():
    rec = {'time': datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),
           'argv': sys.argv[1:]}
    try:
        command, tokens = validate(sys.argv[1:], int(os.environ['IA2_A3B_PORT']))
        rec.update(command=command, tokens=tokens)
        # Only replace the executable token; retain every byte of shell data.
        result = subprocess.run(['/bin/sh', '-c', '/usr/bin/curl' + command[len('curl'):]],
                                env={'PATH': '', 'LC_ALL': 'C', 'CURL_HOME': os.path.dirname(os.environ['IA2_A3B_SSH_LOG'])},
                                capture_output=True, timeout=8)
        rec.update(exit=result.returncode, stdout=result.stdout.decode('utf-8', 'replace'),
                   stderr=result.stderr.decode('utf-8', 'replace'))
        code = result.returncode
    except Exception as exc:
        rec.update(exit=255, rejected=str(exc))
        result, code = None, 255
    with open(os.environ['IA2_A3B_SSH_LOG'], 'a') as f:
        f.write(json.dumps(rec, sort_keys=True) + '\n')
    if result:
        sys.stdout.buffer.write(result.stdout)
        sys.stderr.buffer.write(result.stderr)
    else:
        print(rec['rejected'], file=sys.stderr)
    return code


if __name__ == '__main__':
    sys.exit(main())
