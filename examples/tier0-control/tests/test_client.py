"""ia2client against throw-away socket servers: a body shorter than promised must be an error even when it parses."""
import os
import socket
import sys
import threading
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from bridge.ia2client import Ia2Client  # noqa: E402

FULL = b'{"ok":true,"name":"v","value":5,"undelivered_device":null}'


def serve(payload, hold=0.0):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('127.0.0.1', 0))
    srv.listen(1)

    def run():
        c, _ = srv.accept()
        c.recv(65536)
        c.sendall(payload)
        c.close()
        srv.close()
    threading.Thread(target=run, daemon=True).start()
    return srv.getsockname()[1]


def get(payload):
    return Ia2Client('127.0.0.1', serve(payload), timeout_s=2).request('GET', '/x')


class Client(unittest.TestCase):
    def test_complete_response(self):
        r = get(b'HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n' % len(FULL) + FULL)
        self.assertEqual((r['kind'], r['status'], r['json']['value']), ('response', 200, 5))

    def test_body_shorter_than_promised_is_an_error_even_if_it_parses(self):
        r = get(b'HTTP/1.1 200 OK\r\nContent-Length: 80\r\nConnection: close\r\n\r\n' + FULL)
        self.assertEqual((r['kind'], r['error_kind']), ('error', 'truncated'))

    def test_truncated_mid_json(self):
        r = get(b'HTTP/1.1 200 OK\r\nContent-Length: 200\r\nConnection: close\r\n\r\n{"ok": tr')
        self.assertEqual((r['kind'], r['error_kind']), ('error', 'truncated'))

    def test_oversized_declared_length(self):
        r = get(b'HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\nConnection: close\r\n\r\nx')
        self.assertEqual(r['kind'], 'error')

    def test_redirects_are_returned_not_followed(self):
        r = get(b'HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
        self.assertEqual((r['kind'], r['status']), ('response', 302))

    def test_connection_refused(self):
        s = socket.socket()
        s.bind(('127.0.0.1', 0))
        port = s.getsockname()[1]
        s.close()
        r = Ia2Client('127.0.0.1', port, timeout_s=1).request('GET', '/x')
        self.assertEqual((r['kind'], r['error_kind']), ('error', 'connect'))


if __name__ == '__main__':
    unittest.main()
