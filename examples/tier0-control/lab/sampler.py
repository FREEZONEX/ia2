"""Read-side sampler for the lab.  Same SHAPE as the production read-only collector's `samples` payload
(ia2-tier0-readonly-20261023/payload/app/ia2_ro/collector.py L91-L104) but NOT the same code: that code is fenced to
127.0.0.1:13001 and identifies the process through /proc, neither of which works on this Mac (WP0).  Differences:
identity = the runtime's own runtime_id; no spool/outbox/upload; one sample per advance of scan_count at most.

observed_at_ms is the collector's estimate: (wall clock at first sight) - (scan clock), per runtime instance."""
import hashlib
import http.client
import json
import struct
import threading
import time


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


class Sampler(threading.Thread):
    def __init__(self, port, out_path, interval_s=0.2):
        super().__init__(daemon=True)
        self.port, self.interval_s = port, interval_s
        self.out = open(out_path, 'w', buffering=1)
        self.stop_flag = threading.Event()
        self.paused = threading.Event()        # lab control: while set, nothing is sampled (E1, E2, C8)
        self.anchor, self.last_scan = {}, {}
        self.errors = 0

    def _get_status(self):
        conn = http.client.HTTPConnection('127.0.0.1', self.port, timeout=2.0)
        try:
            conn.request('GET', '/status', headers={'X-IA2-Origin': 'lab-sampler'})
            resp = conn.getresponse()
            return json.loads(resp.read().decode('utf-8')) if resp.status == 200 else None
        finally:
            conn.close()

    def run(self):
        while not self.stop_flag.is_set():
            try:
                st = None if self.paused.is_set() else self._get_status()
                if st and isinstance(st.get('last_snapshot'), dict) and st.get('runtime_id'):
                    snap, rid = st['last_snapshot'], st['runtime_id']
                    now_ms = int(time.time() * 1000)
                    if rid not in self.anchor:
                        self.anchor[rid] = now_ms - snap['timestamp_us'] // 1000
                    if snap['scan_count'] > self.last_scan.get(rid, -1):
                        self.last_scan[rid] = snap['scan_count']
                        rec = {'schema': 'ia2.sample/lab', 'native_runtime_id': rid, 'scan_count': snap['scan_count'],
                               'event_id': hashlib.sha256(('%s:%s' % (rid, snap['scan_count'])).encode()).hexdigest()[:32],
                               'scan_timestamp_us': snap['timestamp_us'],
                               'observed_at_ms': self.anchor[rid] + snap['timestamp_us'] // 1000,
                               'collected_at_ms': now_ms,
                               'variables': [dict(v, decoded_value=decode(v)) for v in snap['vars']],
                               'device_health': st.get('device_health'), 'mode': st.get('mode'), 'fault': st.get('fault'),
                               'watchdog_tripped': st.get('watchdog_tripped'), 'latch_reason': st.get('latch_reason'),
                               'forces': st.get('forces'), 'snapshot_tap': st.get('snapshot_tap'),
                               'time_basis': 'anchored_scan_clock_estimate'}
                        self.out.write(json.dumps(rec, separators=(',', ':')) + '\n')
            except Exception:  # noqa: BLE001 - counted; the runtime may be down on purpose
                self.errors += 1
            self.stop_flag.wait(self.interval_s)
        self.out.close()

    def stop(self):
        self.stop_flag.set()
        self.join(timeout=3)
