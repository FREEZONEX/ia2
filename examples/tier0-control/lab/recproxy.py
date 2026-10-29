"""Recording TCP proxy between the bridge and the runtime: every HTTP request the bridge makes is logged (method, path,
origin label, body) and the status of every response, so a test can say exactly what reached IA2.  Loopback only."""
import asyncio
import json
import time


class RecordingProxy:
    def __init__(self, listen_port, upstream_port, log_path):
        self.listen_port, self.upstream_port = listen_port, upstream_port
        self.log = open(log_path, 'w', buffering=1)
        self.server = None
        self.conns = 0

    def emit(self, **rec):
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
        await asyncio.gather(self._up(conn, creader, uwriter), self._down(conn, ureader, cwriter),
                             return_exceptions=True)

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
                    self.emit(ev='resp', conn=conn, status=int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else None)
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
