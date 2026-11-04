"""Recording TCP proxy between the bridge and the runtime: every HTTP request the bridge makes is logged (method, path,
origin label, body) and the status of every response, so a test can say exactly what reached IA2.  Loopback only.

For the R3 timing cases it also logs the local port of each upstream socket (to find the runtime's end in netstat) and has two
controls that are inert unless a case arms them: `hold_write` (an asyncio.Event) takes the runtime's answer to ONE POST /write,
calls `on_held`, waits for the event and then drops the answer; `keep_write_upstream` leaves the runtime's connection open when
the bridge closes a POST /write connection, so only the client disconnect differs from the timing it controls."""
import asyncio
import json
import time


class RecordingProxy:
    def __init__(self, listen_port, upstream_port, log_path):
        self.listen_port, self.upstream_port = listen_port, upstream_port
        self.log = open(log_path, 'w', buffering=1)
        self.server = None
        self.conns = 0
        self.hold_write = None
        self.on_held = None
        self.keep_write_upstream = False
        self.up_local_port = {}            # conn -> local port of the proxy's socket to the runtime
        self._req_of = {}                  # conn -> (method, path) of its last complete request

    def emit(self, **rec):
        if rec.get('ev') == 'req':
            self._req_of[rec['conn']] = (rec.get('method'), rec.get('path'))
        rec['t'] = round(time.time(), 6)
        self.log.write(json.dumps(rec) + '\n')

    async def start(self):
        self.server = await asyncio.start_server(self._on_connect, '127.0.0.1', self.listen_port, reuse_address=True)

    async def stop(self):
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        self.log.close()

    async def _on_connect(self, creader, cwriter):
        self.conns += 1
        conn = self.conns
        try:
            ureader, uwriter = await asyncio.open_connection('127.0.0.1', self.upstream_port)
        except OSError as exc:
            self.emit(ev='upstream_error', conn=conn, error=repr(exc))
            cwriter.close()
            return
        sock = uwriter.get_extra_info('sockname')
        self.up_local_port[conn] = sock[1] if sock else None
        self.emit(ev='upstream', conn=conn, local_port=self.up_local_port[conn])
        await asyncio.gather(self._up(conn, creader, uwriter), self._down(conn, ureader, cwriter),
                             return_exceptions=True)

    def _is_write(self, conn):
        return self._req_of.get(conn) == ('POST', '/write')

    async def _up(self, conn, reader, writer):
        buf = b''
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                writer.write(data)
                await writer.drain()
                buf += data
                while True:
                    head_end = buf.find(b'\r\n\r\n')
                    if head_end < 0:
                        break
                    head = buf[:head_end].decode('latin1').split('\r\n')
                    headers = {}
                    for line in head[1:]:
                        k, _, v = line.partition(':')
                        headers[k.strip().lower()] = v.strip()
                    length = int(headers.get('content-length', '0') or 0)
                    if len(buf) < head_end + 4 + length:
                        break
                    body = buf[head_end + 4:head_end + 4 + length].decode('utf-8', 'replace')
                    method, path = (head[0].split(' ') + ['', ''])[:2]
                    self.emit(ev='req', conn=conn, method=method, path=path, origin=headers.get('x-ia2-origin'), body=body)
                    buf = buf[head_end + 4 + length:]
        finally:
            if self.keep_write_upstream and self._is_write(conn):
                self.emit(ev='client_closed_upstream_kept_open', conn=conn)
                await asyncio.sleep(10)    # long enough for the runtime to finish the request once it continues
            writer.close()

    async def _down(self, conn, reader, writer):
        first = True
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                if first:
                    first = False
                    parts = data.split(b'\r\n', 1)[0].decode('latin1').split(' ')
                    status = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else None
                    self.emit(ev='resp', conn=conn, status=status)
                    if self.hold_write is not None and self._is_write(conn):
                        gate, self.hold_write = self.hold_write, None
                        self.emit(ev='resp_held', conn=conn, status=status, nbytes=len(data),
                                  body=data.split(b'\r\n\r\n', 1)[-1][:300].decode('utf-8', 'replace'))
                        if self.on_held:
                            self.on_held()
                        await gate.wait()
                        self.emit(ev='resp_dropped', conn=conn)
                        return
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
