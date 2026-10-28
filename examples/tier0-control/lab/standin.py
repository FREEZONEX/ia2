"""Stand-in for the Agent-facing control toolset plus the Tier0 topic: signs requests and appends them to a file, reads the
bridge's trace back.  (The real thing is 1b; this keeps the properties that matter: append-only, anyone-can-write.)"""
import json
import os
import secrets
import time

from bridge import auth  # the signing function is shared; the *verification of outcomes* (lab/verifier.py) is not


def now_ms():
    return int(time.time() * 1000)


class Standin:
    def __init__(self, requests_path, secret, key_id='k1', device_id='bench01-sim', session_id='slice-lab'):
        self.path, self.secret, self.key_id = requests_path, secret, key_id
        self.device_id, self.session_id = device_id, session_id
        open(self.path, 'a').close()

    def payload(self, target_key, target, ttl_ms=20000, **override):
        t = now_ms()
        p = {'event_id': secrets.token_hex(8), 'action_id': secrets.token_hex(16), 'session_id': self.session_id,
             'device_id': self.device_id, 'command': 'ia2_set', 'target_key': target_key, 'target': target,
             'simulation': True, 'requested_at_ms': t, 'deadline_ms': t + ttl_ms, 'requested_by': 'agent:lab'}
        p.update(override)
        return p

    @staticmethod
    def payload_text(payload):
        return json.dumps(payload, sort_keys=True, separators=(',', ':'))

    def envelope(self, payload_text, secret=None, key_id=None):
        return json.dumps({'v': 'ia2.control/v0', 'key_id': key_id or self.key_id, 'payload': payload_text,
                           'sig': auth.sign(secret or self.secret, payload_text)})

    def append_line(self, line):
        with open(self.path, 'a', encoding='utf-8') as f:
            f.write(line + '\n')
            f.flush()
            os.fsync(f.fileno())

    def submit_text(self, payload_text, secret=None, key_id=None):
        """Sign and append EXACTLY these payload bytes (tests that need a payload no honest toolset would produce)."""
        self.append_line(self.envelope(payload_text, secret, key_id))

    def submit(self, target_key, target, ttl_ms=20000, **override):
        """Sign and append; returns the payload dict (it carries the action_id)."""
        p = self.payload(target_key, target, ttl_ms, **override)
        self.append_line(self.envelope(self.payload_text(p)))
        return p


def read_jsonl(path):
    out = []
    if os.path.exists(path):
        with open(path, encoding='utf-8') as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        out.append(json.loads(line))
                    except ValueError:
                        pass
    return out


def read_trace(path):
    seen, out = set(), []
    for e in read_jsonl(path):
        if e.get('event_id') not in seen:
            seen.add(e.get('event_id'))
            out.append(e)
    return out
