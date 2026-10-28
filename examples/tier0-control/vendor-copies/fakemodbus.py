#!/usr/bin/env python3
"""Programmable fake Modbus TCP slave (stdlib only, Python >= 3.8).

Function codes: FC01/02 (all-zero bits), FC03/04 (register read), FC05 (echo), FC06, FC15, FC16.
Anything else gets exception 0x01.  Every request and every mode change is logged with a wall-clock
epoch (time.time(), microsecond resolution) so it can be lined up with the runtime's own log.

Modes (switchable at any instant with FakeSlave.set_mode):
  normal   answer every request, after `delay_ms` (per request, requests on one connection are
           served strictly in order, like a serial slave behind a gateway)
  silent   keep accepting connections and reading requests, never answer.  This is the
           "silent but the link has not been declared down" case: the TCP connection stays up.
  close    close every open connection now; new connections are accepted and closed at once
  refuse   close every open connection and stop listening (connect -> ECONNREFUSED)

A request that is received while in `silent` is logged with action='swallowed'.  A request that was
waiting out its `delay_ms` when the mode flipped to silent/close/refuse is dropped
(action='swallowed_late' / 'dropped_late'), as a device that went away would.

Standalone use (not needed by the rehearsal, kept for manual poking):
  python3 fakemodbus.py --port 13341 --log slave.jsonl [--mode silent] [--delay-ms 50]
"""
import argparse
import asyncio
import json
import struct
import sys
import time

MODES = ('normal', 'silent', 'close', 'refuse')


def _seed_regs():
    # Same addresses as the bench coupler (di_word, ai1, ao1/ao1_echo, system_error).
    regs = {0: 0x00A5, 64: 100, 193: 0, 388: 0}
    return regs


class FakeSlave:
    def __init__(self, name, port, log=None, host='127.0.0.1', delay_ms=0.0):
        self.name = name
        self.host = host
        self.port = port
        self.mode = 'normal'
        self.delay_ms = float(delay_ms)
        self.regs = _seed_regs()
        self._log = log or (lambda rec: None)
        self._server = None
        self._conns = {}          # conn id -> StreamWriter
        self._next_conn = 1
        self._tasks = set()
        self.counters = {'req': 0, 'resp': 0, 'swallowed': 0, 'accept': 0}

    # ---- logging -------------------------------------------------------------------------------
    def emit(self, ev, **fields):
        rec = {'t': round(time.time(), 6), 'slave': self.name, 'ev': ev}
        rec.update(fields)
        self._log(rec)

    # ---- lifecycle -----------------------------------------------------------------------------
    async def start(self):
        await self._listen()

    async def _listen(self):
        # reuse_address so a run that follows another on the same port is not refused by TIME_WAIT.
        self._server = await asyncio.start_server(self._on_connect, self.host, self.port, reuse_address=True)
        self.emit('listen', port=self.port)

    async def stop(self):
        if self._server is not None:
            self._server.close()
            try:
                await self._server.wait_closed()
            except Exception:
                pass
            self._server = None
        self._abort_all('stop')
        for task in list(self._tasks):
            task.cancel()
        if self._tasks:
            await asyncio.gather(*self._tasks, return_exceptions=True)
        self.emit('stopped')

    def _abort_all(self, why):
        for conn_id, writer in list(self._conns.items()):
            try:
                writer.close()
            except Exception:
                pass
            self.emit('closed_by_slave', conn=conn_id, why=why)
        self._conns.clear()

    async def set_mode(self, mode, delay_ms=None):
        if mode not in MODES:
            raise ValueError('bad mode %r' % mode)
        previous = self.mode
        if delay_ms is not None:
            self.delay_ms = float(delay_ms)
        self.mode = mode
        self.emit('mode', mode=mode, previous=previous, delay_ms=self.delay_ms)
        if mode == 'close':
            self._abort_all('mode_close')
        elif mode == 'refuse':
            self._abort_all('mode_refuse')
            if self._server is not None:
                self._server.close()
                try:
                    await self._server.wait_closed()
                except Exception:
                    pass
                self._server = None
                self.emit('unlisten', port=self.port)
        elif self._server is None:
            await self._listen()

    # ---- connection handling -------------------------------------------------------------------
    async def _on_connect(self, reader, writer):
        conn_id = self._next_conn
        self._next_conn += 1
        peer = writer.get_extra_info('peername')
        self.counters['accept'] += 1
        self.emit('accept', conn=conn_id, peer_port=peer[1] if peer else None, mode=self.mode)
        if self.mode in ('close', 'refuse'):
            writer.close()
            self.emit('closed_by_slave', conn=conn_id, why='accept_in_close_mode')
            return
        self._conns[conn_id] = writer
        task = asyncio.current_task()
        self._tasks.add(task)
        try:
            await self._serve(conn_id, reader, writer)
        except (asyncio.CancelledError, asyncio.IncompleteReadError):
            pass
        except Exception as exc:  # peer reset etc.
            self.emit('conn_error', conn=conn_id, error=repr(exc))
        finally:
            self._tasks.discard(task)
            if self._conns.pop(conn_id, None) is not None:
                self.emit('eof', conn=conn_id)     # the client (runtime) went away
            try:
                writer.close()
            except Exception:
                pass

    async def _serve(self, conn_id, reader, writer):
        while True:
            header = await reader.readexactly(7)
            tid, pid, length, unit = struct.unpack('>HHHB', header)
            pdu = await reader.readexactly(length - 1) if length > 1 else b''
            t_req = time.time()
            self.counters['req'] += 1
            info = self._describe(pdu)
            mode = self.mode
            if mode == 'silent':
                self.counters['swallowed'] += 1
                self.emit('req', conn=conn_id, tid=tid, unit=unit, action='swallowed', t_req=round(t_req, 6), **info)
                continue
            if mode in ('close', 'refuse'):
                self.emit('req', conn=conn_id, tid=tid, unit=unit, action='dropped', t_req=round(t_req, 6), **info)
                continue
            if self.delay_ms > 0:
                await asyncio.sleep(self.delay_ms / 1000.0)
                if self.mode != 'normal':
                    self.emit('req', conn=conn_id, tid=tid, unit=unit, t_req=round(t_req, 6),
                              action='swallowed_late' if self.mode == 'silent' else 'dropped_late', **info)
                    continue
            if conn_id not in self._conns:
                return
            response = self._handle(pdu)
            writer.write(struct.pack('>HHHB', tid, pid, len(response) + 1, unit) + response)
            await writer.drain()
            self.counters['resp'] += 1
            self.emit('req', conn=conn_id, tid=tid, unit=unit, action='resp',
                      t_resp=round(time.time(), 6), t_req=round(t_req, 6), **info)

    # ---- Modbus PDU ----------------------------------------------------------------------------
    @staticmethod
    def _describe(pdu):
        if not pdu:
            return {'fc': None}
        fc = pdu[0]
        info = {'fc': fc}
        if fc in (1, 2, 3, 4) and len(pdu) >= 5:
            info['addr'], info['qty'] = struct.unpack('>HH', pdu[1:5])
        elif fc in (5, 6) and len(pdu) >= 5:
            info['addr'], info['value'] = struct.unpack('>HH', pdu[1:5])
        elif fc in (15, 16) and len(pdu) >= 6:
            info['addr'], info['qty'] = struct.unpack('>HH', pdu[1:5])
            if fc == 16 and len(pdu) >= 6 + 2:
                info['value'] = struct.unpack('>H', pdu[6:8])[0]
        return info

    def _handle(self, pdu):
        if not pdu:
            return bytes([0x80, 0x01])
        fc = pdu[0]
        try:
            if fc in (3, 4):
                addr, qty = struct.unpack('>HH', pdu[1:5])
                if not 1 <= qty <= 125:
                    return bytes([fc | 0x80, 0x03])
                words = [self.regs.get(addr + i, 0) & 0xFFFF for i in range(qty)]
                return bytes([fc, qty * 2]) + b''.join(struct.pack('>H', w) for w in words)
            if fc in (1, 2):
                addr, qty = struct.unpack('>HH', pdu[1:5])
                if not 1 <= qty <= 2000:
                    return bytes([fc | 0x80, 0x03])
                nbytes = (qty + 7) // 8
                return bytes([fc, nbytes]) + bytes(nbytes)
            if fc == 5:
                return pdu[:5]
            if fc == 6:
                addr, value = struct.unpack('>HH', pdu[1:5])
                self.regs[addr] = value
                return pdu[:5]
            if fc == 15:
                return pdu[:5]
            if fc == 16:
                addr, qty, nbytes = struct.unpack('>HHB', pdu[1:6])
                for i in range(qty):
                    self.regs[addr + i] = struct.unpack('>H', pdu[6 + 2 * i:8 + 2 * i])[0]
                return pdu[:5]
        except struct.error:
            return bytes([fc | 0x80, 0x03])
        return bytes([fc | 0x80, 0x01])


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--name', default='slave')
    parser.add_argument('--log', default='-')
    parser.add_argument('--mode', default='normal', choices=MODES)
    parser.add_argument('--delay-ms', type=float, default=0.0)
    args = parser.parse_args()
    out = sys.stdout if args.log == '-' else open(args.log, 'w', buffering=1)

    def log(rec):
        out.write(json.dumps(rec) + '\n')

    async def run():
        slave = FakeSlave(args.name, args.port, log=log, delay_ms=args.delay_ms)
        await slave.start()
        if args.mode != 'normal':
            await slave.set_mode(args.mode)
        try:
            while True:
                await asyncio.sleep(3600)
        finally:
            await slave.stop()

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        pass


if __name__ == '__main__':
    main()
