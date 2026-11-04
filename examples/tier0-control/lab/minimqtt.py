"""Minimal MQTT 3.1.1 client in pure Python stdlib (no external dependencies).
Supports CONNECT, CONNACK, PUBLISH (QoS 0/1, retain), PUBACK, SUBSCRIBE (QoS 0/1),
SUBACK, PINGREQ, PINGRESP, DISCONNECT.
Thread-safe message accumulator and wait predicates for test suites."""
import io
import socket
import struct
import threading
import time


def _encode_varlen(n: int) -> bytes:
    buf = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n > 0:
            b |= 0x80
        buf.append(b)
        if n == 0:
            break
    return bytes(buf)


def _decode_varlen_stream(f) -> int:
    multiplier = 1
    value = 0
    while True:
        b = f.read(1)
        if not b:
            raise EOFError('socket closed while reading varlen')
        encoded = b[0]
        if multiplier > 128 ** 3:
            raise ValueError('invalid MQTT remaining length')
        value += (encoded & 0x7F) * multiplier
        multiplier *= 128
        if (encoded & 0x80) == 0:
            break
    return value


def _read_exact(f, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = f.read(n - len(buf))
        if not chunk:
            raise EOFError('socket closed while reading %d bytes (got %d)' % (n, len(buf)))
        buf.extend(chunk)
    return bytes(buf)


class MqttMessage:
    def __init__(self, topic: str, payload: bytes, qos: int, retain: bool, received_at_ms: int):
        self.topic = topic
        self.payload = payload
        self.qos = qos
        self.retain = retain
        self.received_at_ms = received_at_ms

    @property
    def text(self) -> str:
        return self.payload.decode('utf-8', errors='replace')

    def __repr__(self) -> str:
        return f'<MqttMessage topic={self.topic!r} len={len(self.payload)} qos={self.qos} retain={self.retain}>'


class MiniMqttClient:
    def __init__(self, host: str, port: int, client_id: str = None, keepalive: int = 30):
        self.host = host
        self.port = port
        self.client_id = client_id or f'minimqtt-{int(time.time()*1000)%1000000}'
        self.keepalive = keepalive
        self.sock = None
        self.rfile = None
        self._lock = threading.Lock()
        self._rx_thread = None
        self._running = False
        self._packet_id = 0
        self._messages = []
        self.error = None
        self._ping_event = threading.Event()
        self._suback_events = {}
        self._puback_events = {}
        self._msg_cond = threading.Condition(self._lock)

    def _next_pid(self) -> int:
        with self._lock:
            self._packet_id = (self._packet_id % 65535) + 1
            return self._packet_id

    def connect(self, timeout: float = 5.0):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect((self.host, self.port))
        self.rfile = self.sock.makefile('rb')

        # Packet: CONNECT
        proto_name = b'\x00\x04MQTT'
        proto_level = b'\x04'  # 3.1.1
        connect_flags = b'\x02'  # clean session
        keepalive_bytes = struct.pack('!H', self.keepalive)
        vheader = proto_name + proto_level + connect_flags + keepalive_bytes

        cid_bytes = self.client_id.encode('utf-8')
        payload = struct.pack('!H', len(cid_bytes)) + cid_bytes

        pkt = bytes([0x10]) + _encode_varlen(len(vheader) + len(payload)) + vheader + payload
        self.sock.sendall(pkt)

        # Read CONNACK (fixed header 0x20, len 0x02, session present, rc)
        h = _read_exact(self.rfile, 1)
        if h[0] != 0x20:
            raise ConnectionError(f'Expected CONNACK (0x20), got 0x{h[0]:02x}')
        rem_len = _decode_varlen_stream(self.rfile)
        body = _read_exact(self.rfile, rem_len)
        if len(body) < 2 or body[1] != 0:
            rc = body[1] if len(body) >= 2 else -1
            raise ConnectionError(f'MQTT connection refused with code {rc}')

        self.sock.settimeout(None)
        self._running = True
        self._rx_thread = threading.Thread(target=self._rx_loop, name=f'mqtt-rx-{self.client_id}', daemon=True)
        self._rx_thread.start()

    def _rx_loop(self):
        try:
            while self._running:
                b1 = self.rfile.read(1)
                if not b1:
                    break
                pkt_type = (b1[0] >> 4) & 0x0F
                flags = b1[0] & 0x0F
                rem_len = _decode_varlen_stream(self.rfile)
                body = _read_exact(self.rfile, rem_len)

                if pkt_type == 3:  # PUBLISH
                    qos = (flags >> 1) & 0x03
                    retain = bool(flags & 0x01)
                    bio = io.BytesIO(body)
                    topic_len = struct.unpack('!H', bio.read(2))[0]
                    topic = bio.read(topic_len).decode('utf-8', errors='replace')
                    pid = None
                    if qos > 0:
                        pid = struct.unpack('!H', bio.read(2))[0]
                    payload = bio.read()

                    msg = MqttMessage(topic, payload, qos, retain, int(time.time() * 1000))
                    with self._lock:
                        self._messages.append(msg)
                        self._msg_cond.notify_all()

                    if qos == 1 and pid is not None:
                        # Send PUBACK
                        ack = bytes([0x40, 0x02]) + struct.pack('!H', pid)
                        try:
                            with self._lock:
                                self.sock.sendall(ack)
                        except OSError:
                            pass

                elif pkt_type == 4:  # PUBACK
                    if len(body) >= 2:
                        pid = struct.unpack('!H', body[:2])[0]
                        ev = self._puback_events.get(pid)
                        if ev:
                            ev.set()

                elif pkt_type == 9:  # SUBACK
                    if len(body) >= 3:
                        if body[2:] not in (b'\x00', b'\x01'):
                            raise ConnectionError('subscription refused: %r' % body)
                        pid = struct.unpack('!H', body[:2])[0]
                        ev = self._suback_events.get(pid)
                        if ev:
                            ev.set()

                elif pkt_type == 12:  # PINGREQ
                    try:
                        self.sock.sendall(bytes([0xD0, 0x00]))
                    except OSError:
                        pass

                elif pkt_type == 13:  # PINGRESP
                    self._ping_event.set()

        except Exception as exc:
            if self._running:
                self.error = repr(exc)
        finally:
            self._running = False

    def ping(self, timeout=3):
        self._ping_event.clear()
        with self._lock:
            self.sock.sendall(b'\xc0\x00')
        if not self._ping_event.wait(timeout) or not self._running:
            raise ConnectionError('MQTT observer is not live: %s' % self.error)

    def subscribe(self, topic: str, qos: int = 0, timeout: float = 5.0):
        pid = self._next_pid()
        ev = threading.Event()
        self._suback_events[pid] = ev

        vheader = struct.pack('!H', pid)
        t_bytes = topic.encode('utf-8')
        payload = struct.pack('!H', len(t_bytes)) + t_bytes + bytes([qos & 0x03])

        pkt = bytes([0x82]) + _encode_varlen(len(vheader) + len(payload)) + vheader + payload
        with self._lock:
            self.sock.sendall(pkt)

        if not ev.wait(timeout):
            raise TimeoutError(f'Timed out waiting for SUBACK on topic {topic}')
        self._suback_events.pop(pid, None)

    def publish(self, topic: str, payload, qos: int = 0, retain: bool = False, timeout: float = 5.0):
        if isinstance(payload, str):
            payload_bytes = payload.encode('utf-8')
        elif isinstance(payload, (bytes, bytearray)):
            payload_bytes = bytes(payload)
        else:
            raise TypeError('payload must be str or bytes')

        t_bytes = topic.encode('utf-8')
        vheader = struct.pack('!H', len(t_bytes)) + t_bytes
        pid = None
        ev = None
        if qos > 0:
            pid = self._next_pid()
            vheader += struct.pack('!H', pid)
            ev = threading.Event()
            self._puback_events[pid] = ev

        flags = (1 if retain else 0) | ((qos & 0x03) << 1)
        pkt_hdr = bytes([0x30 | flags])
        pkt = pkt_hdr + _encode_varlen(len(vheader) + len(payload_bytes)) + vheader + payload_bytes

        with self._lock:
            self.sock.sendall(pkt)

        if qos > 0 and ev:
            if not ev.wait(timeout):
                raise TimeoutError(f'Timed out waiting for PUBACK on topic {topic}')
            self._puback_events.pop(pid, None)

    def messages(self, topic_filter: str = None):
        with self._lock:
            if not topic_filter:
                return list(self._messages)
            if topic_filter.endswith('/#'):
                root = topic_filter[:-2]
                return [m for m in self._messages if m.topic == root or m.topic.startswith(root + '/')]
            return [m for m in self._messages if m.topic == topic_filter]

    def wait_for_message(self, topic: str = None, predicate=None, timeout: float = 5.0):
        end = time.monotonic() + timeout
        with self._msg_cond:
            while time.monotonic() < end:
                for m in self._messages:
                    if topic is not None:
                        if topic.endswith('/#'):
                            root = topic[:-2]
                            if not (m.topic == root or m.topic.startswith(root + '/')):
                                continue
                        elif m.topic != topic:
                            continue
                    if predicate is None or predicate(m):
                        return m
                rem = end - time.monotonic()
                if rem <= 0:
                    break
                self._msg_cond.wait(timeout=min(rem, 0.1))
        raise TimeoutError(f'Message on {topic} matching predicate did not arrive within {timeout}s')

    def disconnect(self):
        self._running = False
        try:
            if self.sock:
                self.sock.sendall(bytes([0xE0, 0x00]))
        except OSError:
            pass
        self.close()

    def close(self):
        self._running = False
        try:
            if self.sock:
                self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        if self._rx_thread and self._rx_thread.is_alive():
            self._rx_thread.join(timeout=1.0)
        if self.rfile:
            self.rfile.close()
        if self.sock:
            self.sock.close()
