"""Independent, signed, loopback-only GET /status sampler. No bridge imports."""
import argparse
import fcntl
import hashlib
import http.client
import json
import math
import os
import signal
import struct
import sys
import threading
import time

from . import auth


def decode(var):
    bits, kind = var['bits'], var['type_name'].upper()
    signed = {'SINT': 8, 'INT': 16, 'DINT': 32, 'LINT': 64}
    unsigned = {'USINT': 8, 'UINT': 16, 'UDINT': 32, 'ULINT': 64, 'BYTE': 8, 'WORD': 16, 'DWORD': 32, 'LWORD': 64}
    if kind in signed:
        w = signed[kind]
        v = bits & ((1 << w) - 1)
        return v - (1 << w) if v & (1 << (w - 1)) else v
    if kind in unsigned:
        return bits & ((1 << unsigned[kind]) - 1)
    if kind == 'BOOL':
        return bool(bits & 1)
    if kind == 'REAL':
        return struct.unpack('<f', struct.pack('<I', bits & 0xffffffff))[0]
    if kind == 'LREAL':
        return struct.unpack('<d', struct.pack('<Q', bits & 0xffffffffffffffff))[0]
    return None



class Collector:
    def __init__(self, port, out_path, keys, revoked=()):
        self.port, self.keys, self.revoked = port, keys, revoked
        self.anchor, self.last_scan = {}, {}
        fd = os.open(out_path, os.O_RDWR | os.O_CREAT | os.O_APPEND, 0o600)
        self.out = os.fdopen(fd, 'a+b', buffering=0)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.out.seek(0, os.SEEK_END)
            end = self.out.tell()
            self.out.seek(max(0, end - auth.MAX_ENVELOPE - 1))
            tail = self.out.read()
            if tail and not tail.endswith(b'\n'):
                cut = tail.rfind(b'\n')
                if cut < 0 and end > len(tail):
                    raise ValueError('sample tail exceeds recovery bound')
                self.out.truncate(end - len(tail) + cut + 1)
        except BaseException:
            self.out.close()
            raise

    def close(self):
        self.out.close()

    def poll(self):
        conn = http.client.HTTPConnection('127.0.0.1', self.port, timeout=2.0)
        try:
            conn.request('GET', '/status', headers={'X-IA2-Origin': 'control-sampler'})
            resp = conn.getresponse()
            if resp.status != 200:
                raise ValueError('runtime status unavailable')
            st = auth.loads(resp.read(auth.MAX_ENVELOPE + 1))
        finally:
            conn.close()
        if not isinstance(st, dict) or not isinstance(st.get('last_snapshot'), dict) or not st.get('runtime_id'):
            return False
        snap, rid = st['last_snapshot'], st['runtime_id']
        scan = snap['scan_count']
        if scan <= self.last_scan.get(rid, -1):
            return False
        now = int(time.time() * 1000)
        self.anchor.setdefault(rid, now - snap['timestamp_us'] // 1000)
        variables = []
        for v in snap['vars']:
            value = decode(v)
            if isinstance(value, float) and not math.isfinite(value):
                value = None  # invalid numeric feedback can never prove a target
            variables.append(dict(v, decoded_value=value))
        rec = {'schema': 'ia2.sample/lab', 'native_runtime_id': rid, 'scan_count': scan,
               'event_id': hashlib.sha256(('%s:%s' % (rid, scan)).encode()).hexdigest()[:32],
               'scan_timestamp_us': snap['timestamp_us'],
               'observed_at_ms': self.anchor[rid] + snap['timestamp_us'] // 1000,
               'collected_at_ms': now, 'variables': variables,
               'device_health': st.get('device_health'), 'mode': st.get('mode'), 'fault': st.get('fault'),
               'watchdog_tripped': st.get('watchdog_tripped'), 'latch_reason': st.get('latch_reason'),
               'forces': st.get('forces'), 'snapshot_tap': st.get('snapshot_tap'),
               'time_basis': 'anchored_scan_clock_estimate'}
        data = (json.dumps(auth.sign(rec, self.keys, 'sample', self.revoked), separators=(',', ':')) + '\n').encode('ascii')
        self.out.seek(0, os.SEEK_END)
        offset = self.out.tell()
        try:
            remaining = memoryview(data)
            while remaining:
                n = self.out.write(remaining)
                if not n:
                    raise OSError('sample write made no progress')
                remaining = remaining[n:]
        except BaseException:
            self.out.truncate(offset)
            raise
        self.last_scan[rid] = scan
        return True


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--runtime', required=True, help='127.0.0.1:port')
    ap.add_argument('--samples', required=True)
    ap.add_argument('--sample-keys', required=True)
    ap.add_argument('--revoked-keys')
    ap.add_argument('--interval-ms', type=int, default=200)
    args = ap.parse_args(argv)
    try:
        host, port = args.runtime.split(':')
        if host != '127.0.0.1' or not 1 <= int(port) <= 65535 or not 50 <= args.interval_ms <= 60000:
            raise ValueError('invalid loopback address or sampling interval')
        keys = auth.load_keys(args.sample_keys)
        revoked = auth.load_revoked(args.revoked_keys)
        auth.select_key(keys, int(time.time() * 1000), revoked)
        collector = Collector(int(port), args.samples, keys, revoked)
    except (ValueError, OSError):
        print('sampler startup refused: check loopback address, paths, keys and interval', file=sys.stderr)
        return 2
    stop = threading.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: stop.set())
    print(json.dumps({'ready': True, 'pid': os.getpid()}), flush=True)
    errors = 0
    try:
        while not stop.is_set():
            started = time.monotonic()
            try:
                collector.poll()
            except (ValueError, OSError, KeyError, TypeError, http.client.HTTPException):
                errors += 1
                if errors & (errors - 1) == 0:
                    print('sampler failures: %d (no sample published)' % errors, file=sys.stderr)
            stop.wait(max(0, args.interval_ms / 1000 - (time.monotonic() - started)))
    finally:
        collector.close()
    return 0
