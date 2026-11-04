"""D6: the bridge refuses to run against a project whose northbound.toml enables MQTT writes.

The runtime exposes no northbound state over HTTP. Parse the deployed TOML with
the bundled TOML 1.0 parser, including quoted keys, strings and inline tables.
An omitted mqtt/allow_write uses the runtime's disabled default; an explicit
value must be the boolean false, even if mqtt.enabled is false. Malformed TOML,
wrong field types and unreadable files refuse. A missing file is northbound off.
"""
import hashlib
import os

from .errors import Reject
from ._vendor.tomli import loads, TOMLDecodeError


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
    try:
        doc = loads(text)
    except (TOMLDecodeError, RecursionError):
        # Parser diagnostics may include configuration values (e.g. passwords).
        raise Reject('northbound_write_enabled', 'northbound.toml is not valid TOML') from None
    mqtt = doc.get('mqtt', {})
    if not isinstance(mqtt, dict) or mqtt.get('allow_write', False) is not False:
        raise Reject('northbound_write_enabled', 'northbound.toml does not disable mqtt.allow_write')
    return hashlib.sha256(data).hexdigest()
