"""HTTP client for the four IA2 routes the bridge uses.  Strict: no redirects are followed, a truncated body is an
error, and every result records exactly what was sent and received (for the journal).

ONE budget per HTTP operation (P1, F10).  It is measured on the monotonic clock from the start of the work (connect) to the last byte of
the body, and every connect, send and receive on the socket is armed with what is LEFT of it -- a per-read socket timeout lets a peer that
answers one byte at a time keep the bridge for as long as it likes.  The budget is per operation (GET 2 s, POST 3 s by default), not for
a whole action, and nothing here promises hard real time: it bounds what the peer can do to the bridge's time.

Sizes are bounded while the bytes arrive, each with its own limit: the status line and header block (MAX_HEAD), the chunk-size lines and
trailers of a chunked body together (MAX_META), and the body (MAX_BODY).  What is buffered never exceeds a limit by more than one read block.

A timeout only says the outcome is unknown: the connection is closed and the work stops.  Nothing is retried, and a POST is never sent twice."""
import http.client
import io
import json
import re
import socket
import time

from . import strictjson

MAX_BODY = 2 * 1024 * 1024
MAX_HEAD = 16 * 1024            # status line and header block of one answer, bytes
MAX_META = 16 * 1024            # all chunk-size lines (with extensions) and trailers of one chunked body, bytes
MAX_INFO = 8                    # interim (1xx) answers skipped before the real one
READ_BLOCK = 16 * 1024          # one receive
STATUS_LINE = re.compile(r'^HTTP/1\.[01] (\d{3})(?: .*)?$')
TOKEN = re.compile(r'^[!-~]+$')
TEXT = re.compile(r'^[ -~]*$')
HEX = re.compile(rb'^[0-9a-fA-F]{1,8}$')


def _now_ms():
    return int(time.time() * 1000)


class TooLarge(http.client.HTTPException):
    """An answer past one of the limits above, stopped as soon as the bytes that exceed it arrive."""


class Deadline:
    """One budget on a monotonic clock.  `left()` is what each socket operation is armed with; at or past the end it raises the timeout a
    socket would, so a late peer and an exhausted budget look the same to the caller.  An operation may finish AT the deadline; anything
    that still has to be done after it fails."""

    def __init__(self, budget_s, clock):
        self.clock, self.end = clock, clock() + budget_s

    def left(self):
        left = self.end - self.clock()
        if left <= 0:
            raise socket.timeout('the HTTP budget is used up')
        return left


def connect_tcp(host, port, deadline):
    """A connected socket, inside the budget.  (Name lookup cannot be interrupted; the bridge only talks to a loopback address.)"""
    last = None
    for family, kind, proto, _, addr in socket.getaddrinfo(host, port, type=socket.SOCK_STREAM):
        sock = socket.socket(family, kind, proto)
        try:
            sock.settimeout(deadline.left())
            sock.connect(addr)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            return sock
        except socket.timeout:
            sock.close()
            raise
        except OSError as exc:
            sock.close()
            last = exc
    raise last or OSError('no address for %s' % host)


class _Reader:
    """Buffered receives; every refill is armed with the time that is left."""

    def __init__(self, sock, deadline):
        self.sock, self.deadline, self.buf = sock, deadline, bytearray()

    def fill(self):
        self.sock.settimeout(self.deadline.left())
        chunk = self.sock.recv(READ_BLOCK)
        if not chunk:
            return False
        self.buf += chunk
        return True

    def head(self):
        """(status, headers) of the final answer.  Interim (1xx) answers are skipped; status line and headers together stay within MAX_HEAD."""
        spent = 0
        for _ in range(MAX_INFO + 1):
            while True:
                # Each line ends in CRLF or LF, including the final empty line.
                # Use the same boundary for mixed endings as the header parser.
                end_match = re.search(rb'\n\r?\n', self.buf)
                if end_match:
                    end = end_match.end()
                    break
                if len(self.buf) > MAX_HEAD:
                    raise TooLarge('the answer\'s head is larger than %d bytes' % MAX_HEAD)
                if not self.fill():
                    if not self.buf:
                        raise http.client.RemoteDisconnected('Remote end closed connection without response')
                    raise http.client.IncompleteRead(bytes(self.buf))
            spent += end
            if spent > MAX_HEAD:
                raise TooLarge('the answer\'s head is larger than %d bytes' % MAX_HEAD)
            block = bytes(self.buf[:end])
            del self.buf[:end]
            # The email parser behind parse_headers also treats bare CR as a
            # newline, silently hiding later fields in its payload. HTTP does not.
            # Validate only the head: arbitrary CR bytes in the body are data.
            if b'\r' in block.replace(b'\r\n', b''):
                raise http.client.HTTPException('bare CR in response head')
            first, _, rest = block.partition(b'\n')
            line = first.decode('iso-8859-1').rstrip('\r')
            m = STATUS_LINE.match(line)
            if not m:
                raise http.client.BadStatusLine(line)
            status = int(m.group(1))
            headers = http.client.parse_headers(io.BytesIO(rest))
            if headers.defects:
                raise http.client.HTTPException('malformed response headers')
            # A header-only block must not lose lines as email body or a Unix
            # mailbox separator. Neither necessarily appears in defects.
            if headers.get_payload() or headers.get_unixfrom() is not None:
                raise http.client.HTTPException('unconsumed response headers')
            if 100 <= status < 200 and status != 101:
                continue
            return status, headers
        raise http.client.HTTPException('too many interim answers')

    def body(self, headers):
        # Never choose the first of conflicting framing fields. get_all is case-insensitive.
        # Even identical duplicate lengths are rejected, matching the P0 client's conservative policy.
        codings = headers.get_all('transfer-encoding', [])
        lengths = headers.get_all('content-length', [])
        if len(codings) > 1 or len(lengths) > 1 or (codings and lengths):
            raise http.client.HTTPException('ambiguous response framing')
        coding = codings[0] if codings else None
        if coding is not None:
            if [c.strip().lower() for c in coding.split(',')] != ['chunked']:
                raise http.client.HTTPException('unsupported Transfer-Encoding %r' % coding)
            return self._chunked()
        declared = lengths[0] if lengths else None
        if declared is not None:
            declared = declared.strip()
            if not (declared.isascii() and declared.isdigit()) or int(declared) > MAX_BODY:
                raise http.client.HTTPException('bad or oversized Content-Length')
            return self._exact(int(declared))
        while self.fill():                               # no length: the body ends when the peer closes
            if len(self.buf) > MAX_BODY:
                raise TooLarge('the body is larger than %d bytes' % MAX_BODY)
        return bytes(self.buf)

    def _exact(self, n):
        while len(self.buf) < n:
            if not self.fill():
                raise http.client.IncompleteRead(bytes(self.buf), n - len(self.buf))
        data = bytes(self.buf[:n])
        del self.buf[:n]
        return data

    def _line(self, room):
        while True:
            i = self.buf.find(b'\n')
            if i >= 0:
                if i + 1 > room:
                    raise TooLarge('chunk metadata is larger than %d bytes' % MAX_META)
                line = bytes(self.buf[:i + 1])
                del self.buf[:i + 1]
                return line
            if len(self.buf) > room:
                raise TooLarge('chunk metadata is larger than %d bytes' % MAX_META)
            if not self.fill():
                raise http.client.IncompleteRead(bytes(self.buf))

    def _chunked(self):
        body, meta = bytearray(), 0
        while True:
            line = self._line(MAX_META - meta)
            meta += len(line)
            size_text = line.split(b';', 1)[0].strip()
            if not HEX.match(size_text):
                raise http.client.HTTPException('bad chunk size')
            size = int(size_text, 16)
            if size == 0:
                break
            if len(body) + size > MAX_BODY:
                raise TooLarge('the body is larger than %d bytes' % MAX_BODY)
            body += self._exact(size)
            if self._exact(2) != b'\r\n':
                raise http.client.HTTPException('bad chunk terminator')
        while True:                                       # trailers, up to the blank line
            try:
                line = self._line(MAX_META - meta)
            except http.client.IncompleteRead as exc:
                if exc.partial:
                    raise
                return bytes(body)                        # the peer closed after the last chunk without a blank line: the body is whole
            meta += len(line)
            if line in (b'\r\n', b'\n'):
                return bytes(body)


def read_answer(sock, deadline):
    """(status, headers, body bytes) of one answer, everything inside the budget and the size limits."""
    reader = _Reader(sock, deadline)
    status, headers = reader.head()
    if status in (204, 304):
        return status, headers, b''
    return status, headers, reader.body(headers)


class Ia2Client:
    """`timeout_s` / `get_timeout_s` are the budgets of one POST / one GET.  `clock` (monotonic seconds) and `connect` (host, port, deadline -> a
    socket-like object) can be replaced to test the budget without waiting."""

    def __init__(self, host, port, timeout_s=3.0, get_timeout_s=2.0, clock=time.monotonic, connect=connect_tcp):
        self.host, self.port, self.timeout_s, self.get_timeout_s = host, port, timeout_s, get_timeout_s
        self.clock, self.connect = clock, connect

    def request(self, method, path, body=None, origin=None, timeout_s=None):
        data = None
        headers = {}
        if origin:
            headers['X-IA2-Origin'] = origin
        if body is not None:
            data = json.dumps(body, separators=(',', ':')).encode('utf-8')
            headers['Content-Type'] = 'application/json'
        rec = {'method': method, 'path': path, 'req_body': None if data is None else data.decode('utf-8'),
               'origin': origin, 't_send_ms': _now_ms()}
        deadline = Deadline(timeout_s or self.timeout_s, self.clock)
        sock = None
        try:
            sock = self.connect(self.host, self.port, deadline)
            sock.settimeout(deadline.left())
            sock.sendall(self._request_bytes(method, path, data, headers))
            status, answer, raw = read_answer(sock, deadline)
            rec.update(kind='response', status=status, content_type=answer.get('content-type'),
                       text=raw.decode('utf-8', 'replace'), t_recv_ms=_now_ms())
            try:
                rec['json'] = strictjson.loads(rec['text']) if rec['text'] else None
            except strictjson.StrictJSONError:
                rec['json'] = None
            return rec
        except socket.timeout:
            return self._err(rec, 'timeout')
        except ConnectionRefusedError:
            return self._err(rec, 'connect')
        except http.client.IncompleteRead:
            return self._err(rec, 'truncated')
        except (ConnectionResetError, ConnectionAbortedError, BrokenPipeError, http.client.RemoteDisconnected):
            return self._err(rec, 'reset')
        except (http.client.HTTPException, ValueError) as exc:
            return self._err(rec, 'protocol', str(exc))
        except OSError as exc:
            return self._err(rec, 'other', repr(exc))
        finally:
            if sock is not None:
                sock.close()

    def _request_bytes(self, method, path, data, headers):
        if not TOKEN.match(method) or not TOKEN.match(path) or not all(TOKEN.match(k) and TEXT.match(v) for k, v in headers.items()):
            raise ValueError('characters that cannot go into a request line or a header')
        lines = ['%s %s HTTP/1.1' % (method, path), 'Host: %s:%d' % (self.host, self.port), 'Accept-Encoding: identity', 'Connection: close']
        lines += ['%s: %s' % kv for kv in headers.items()]
        if data is not None:
            lines.append('Content-Length: %d' % len(data))
        return ('\r\n'.join(lines) + '\r\n\r\n').encode('latin-1') + (data or b'')

    @staticmethod
    def _err(rec, kind, text=''):
        rec.update(kind='error', error_kind=kind, error_text=text, t_recv_ms=_now_ms())
        return rec

    def get_json(self, path):
        """GET and require a 200 with a JSON body; returns (parsed_or_None, record)."""
        rec = self.request('GET', path, timeout_s=self.get_timeout_s)
        if rec['kind'] == 'response' and rec['status'] == 200 and rec.get('json') is not None:
            return rec['json'], rec
        return None, rec


def journal_view(rec, cap=4096):
    """The part of a result that goes into the journal."""
    keep = ('method', 'path', 'req_body', 'origin', 'kind', 'status', 'content_type', 'error_kind', 'error_text',
            't_send_ms', 't_recv_ms')
    out = {k: rec.get(k) for k in keep if k in rec}
    if rec.get('text') is not None:
        out['text'] = rec['text'][:cap]
        out['text_truncated'] = len(rec['text']) > cap
    return out
