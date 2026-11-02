"""Failure boundaries that can otherwise turn an observation into a false PASS."""
import os
import copy
import json
from pathlib import Path
import socket
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'lab'))
from minimqtt import MiniMqttClient, MqttMessage, _decode_varlen_stream, _read_exact
from test_broker import TempBroker
from cases_c11 import mqtt_window_evidence


class RuntimeObservationTests(unittest.TestCase):
    def setUp(self):
        self.prior = dict(runtime_id='new-instance', last_snapshot=dict(scan_count=10, timestamp_us=100000))
        self.final = dict(runtime_id='new-instance', last_snapshot=dict(scan_count=210, timestamp_us=2100000))
        self.messages = [MqttMessage('test/status', b'online', 1, False, 900)]
        for n in range(1, 11):
            payload = dict(scan=10 + n * 20, ts_us=100000 + n * 200000,
                           values=dict(sp_flow=0.0, sp_flow_fb=0.0), inputs=dict(sp_flow_fb=dict(stale=False)))
            self.messages.append(MqttMessage('test/snapshot', json.dumps(payload).encode(), 1, False, 1000 + n * 200))

    def result(self):
        return mqtt_window_evidence(self.messages, 'test', 1000, 3000, self.prior, self.final)

    def test_advancing_live_runtime_is_a_valid_negative_observation(self):
        self.assertTrue(self.result()['ok'])

    def test_live_observer_without_runtime_snapshots_is_not_evidence(self):
        self.messages = self.messages[:1]
        self.assertFalse(self.result()['ok'])

    def test_offline_or_degraded_then_recovery_invalidates_the_window(self):
        for status in (b'offline', b'degraded'):
            with self.subTest(status=status):
                messages = self.messages + [MqttMessage('test/status', status, 1, False, 1800),
                                           MqttMessage('test/status', b'online', 1, False, 1900)]
                result = mqtt_window_evidence(messages, 'test', 1000, 3000, self.prior, self.final)
                self.assertFalse(result['ok'])

    def test_offline_before_publish_is_not_hidden_by_old_online(self):
        self.messages.insert(1, MqttMessage('test/status', b'offline', 1, False, 950))
        self.assertFalse(self.result()['ok'])

    def test_an_intermediate_snapshot_gap_is_not_hidden_by_late_recovery(self):
        self.messages = [m for m in self.messages if not 1400 <= m.received_at_ms <= 2200]
        self.assertFalse(self.result()['ok'])

    def test_repeated_snapshot_payload_does_not_refresh_liveness(self):
        for message in self.messages[1:]:
            message.payload = self.messages[1].payload
        self.assertFalse(self.result()['ok'])

    def test_retained_snapshots_cannot_establish_liveness(self):
        for message in self.messages[1:]:
            message.retain = True
        self.assertFalse(self.result()['ok'])

    def test_malformed_snapshot_does_not_count_as_runtime_activity(self):
        for payload in (b'{}', b'not JSON', b'null', b'[]'):
            with self.subTest(payload=payload):
                self.messages[1].payload = payload
                self.assertFalse(self.result()['ok'])

    def test_other_instance_or_stale_feedback_is_rejected(self):
        for change in ('identity', 'scan', 'timestamp', 'stale'):
            with self.subTest(change=change):
                prior, final, messages = copy.deepcopy((self.prior, self.final, self.messages))
                body = json.loads(messages[1].payload)
                if change == 'identity':
                    final['runtime_id'] = 'another-instance'
                elif change == 'scan':
                    body['scan'] = final['last_snapshot']['scan_count'] + 1
                elif change == 'timestamp':
                    body['ts_us'] = final['last_snapshot']['timestamp_us'] + 1
                else:
                    body['inputs']['sp_flow_fb']['stale'] = True
                messages[1].payload = json.dumps(body).encode()
                self.assertFalse(mqtt_window_evidence(messages, 'test', 1000, 3000, prior, final)['ok'])


class MqttObserverTests(unittest.TestCase):
    def exchange(self, final_packet, action):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            finished = threading.Event()
            def serve():
                with listener.accept()[0] as conn:
                    conn.settimeout(2)
                    with conn.makefile('rb') as f:
                        _read_exact(f, 1)
                        _read_exact(f, _decode_varlen_stream(f))
                        conn.sendall(b'\x20\x02\x00\x00')
                        if final_packet:
                            _read_exact(f, 1)
                            body = _read_exact(f, _decode_varlen_stream(f))
                            # The subscription's actual packet ID is echoed.
                            conn.sendall(b'\x90\x03' + body[:2] + final_packet)
                        finished.wait(2)
            thread = threading.Thread(target=serve)
            thread.start()
            client = MiniMqttClient('127.0.0.1', listener.getsockname()[1], 'observer-test')
            try:
                client.connect()
                action(client)
            finally:
                client.disconnect()
                finished.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())
            self.assertFalse(client._rx_thread.is_alive())

    def test_rejected_subscription_cannot_start_observation(self):
        def action(client):
            with self.assertRaises(TimeoutError):
                client.subscribe('test/#', qos=1, timeout=0.15)
            self.assertIn('subscription refused', client.error)
        self.exchange(b'\x80', action)

    def test_unresponsive_broker_cannot_pass_liveness_check(self):
        def action(client):
            with self.assertRaises(ConnectionError):
                client.ping(timeout=0.1)
        self.exchange(None, action)

    def test_missing_publish_ack_is_not_reported_as_delivery(self):
        def action(client):
            with self.assertRaises(TimeoutError):
                client.publish('test/write', '{}', qos=1, timeout=0.1)
        self.exchange(None, action)


class BrokerLifecycleTests(unittest.TestCase):
    def helper(self, body, timeout):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'broker'
            path.write_text('#!%s\n%s\n' % (sys.executable, body))
            path.chmod(0o700)
            broker = TempBroker(str(path), 12345, str(Path(directory) / 'broker.log'))
            before = time.monotonic()
            try:
                with self.assertRaises((TimeoutError, RuntimeError)):
                    broker.start(timeout=timeout)
                self.assertIsNotNone(broker.proc.poll())
                self.assertLess(time.monotonic() - before, 5)
            finally:
                broker.stop()

    def test_readiness_timeout_reaps_own_child(self):
        self.helper('import time; time.sleep(30)', 0.1)

    def test_startup_exit_does_not_hang(self):
        self.helper('raise SystemExit(7)', 1)


if __name__ == '__main__':
    unittest.main()
