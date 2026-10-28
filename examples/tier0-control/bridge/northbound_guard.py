"""D6: the bridge refuses to run against a project whose northbound.toml enables MQTT writes.

The runtime exposes no northbound state over HTTP, and Python 3.9 has no TOML parser, so this does not parse: it
looks at every non-comment line that mentions allow_write and demands the literal `false`.  Anything else (true,
odd spellings, an unreadable file) refuses.  A missing file means northbound is off (as the runtime treats it)."""
import hashlib
import os
import re

from .errors import Reject

FALSE_FORM = re.compile(r'allow_write["\']?\s*=\s*false\s*(,|\}|$)')


def fingerprint(project_dir):
    path = os.path.join(project_dir, 'northbound.toml')
    try:
        with open(path, 'rb') as f:
            return hashlib.sha256(f.read()).hexdigest()
    except FileNotFoundError:
        return 'absent'
    except OSError:
        return 'unreadable'


def check(project_dir):
    path = os.path.join(project_dir, 'northbound.toml')
    try:
        with open(path, 'rb') as f:
            data = f.read()
        text = data.decode('utf-8')
    except FileNotFoundError:
        return 'absent'
    except (OSError, UnicodeDecodeError):
        raise Reject('northbound_write_enabled', 'northbound.toml is unreadable') from None
    for line in text.splitlines():
        code = line.split('#', 1)[0]
        if 'allow_write' in code and not FALSE_FORM.search(code):
            raise Reject('northbound_write_enabled', 'northbound.toml does not say allow_write = false')
    return hashlib.sha256(data).hexdigest()
