"""FakeSlave plus coils and fuller write logging (DESIGN-1A gaps F-2, F-3).  Subclass only; vendor-copies/fakemodbus.py is untouched.

  FC01/FC02 read coils / discrete inputs from one coil table (bit i of the reply = coil addr+i)
  FC05 write single coil (0xFF00 on, 0x0000 off; anything else is exception 0x03);  FC15 write multiple coils
  the log of a write carries `words` (every register written by FC16) or `bits` (coils written by FC15)."""
import os
import struct
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'vendor-copies'))
from fakemodbus import FakeSlave  # noqa: E402


class FakeSlavePlus(FakeSlave):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.coils = {}

    @staticmethod
    def _describe(pdu):
        info = FakeSlave._describe(pdu)
        fc = pdu[0] if pdu else None
        try:
            if fc == 16 and len(pdu) >= 6:
                qty = struct.unpack('>H', pdu[3:5])[0]
                info['words'] = [struct.unpack('>H', pdu[6 + 2 * i:8 + 2 * i])[0] for i in range(qty) if len(pdu) >= 8 + 2 * i]
            elif fc == 15 and len(pdu) >= 6:
                qty = struct.unpack('>H', pdu[3:5])[0]
                data = pdu[6:]
                info['bits'] = [(data[i // 8] >> (i % 8)) & 1 for i in range(qty) if i // 8 < len(data)]
        except struct.error:
            pass
        return info

    def _handle(self, pdu):
        if not pdu or pdu[0] not in (1, 2, 5, 15):
            return super()._handle(pdu)
        fc = pdu[0]
        try:
            if fc in (1, 2):
                addr, qty = struct.unpack('>HH', pdu[1:5])
                if not 1 <= qty <= 2000:
                    return bytes([fc | 0x80, 0x03])
                nbytes = (qty + 7) // 8
                out = bytearray(nbytes)
                for i in range(qty):
                    if self.coils.get(addr + i, 0):
                        out[i // 8] |= 1 << (i % 8)
                return bytes([fc, nbytes]) + bytes(out)
            if fc == 5:
                addr, value = struct.unpack('>HH', pdu[1:5])
                if value not in (0x0000, 0xFF00):
                    return bytes([fc | 0x80, 0x03])
                self.coils[addr] = 1 if value else 0
                return pdu[:5]
            addr, qty, nbytes = struct.unpack('>HHB', pdu[1:6])
            data = pdu[6:6 + nbytes]
            if not 1 <= qty <= 1968 or len(data) < (qty + 7) // 8:
                return bytes([fc | 0x80, 0x03])
            for i in range(qty):
                self.coils[addr + i] = (data[i // 8] >> (i % 8)) & 1
            return pdu[:5]
        except struct.error:
            return bytes([fc | 0x80, 0x03])
