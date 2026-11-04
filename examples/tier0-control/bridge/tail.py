"""Reads the requests file in bounded chunks and cuts it into lines of at most `max_line` bytes (W8).  A longer line is not kept: only its
length and sha256 are reported, once, when its newline arrives.  State survives a line that spans several polls."""
import hashlib

CHUNK = 1 << 20


class RequestTail:
    def __init__(self, path, max_line=65536):
        self.path, self.max_line = path, max_line
        self.offset = 0
        self._buf, self._over, self._len, self._h = b'', False, 0, None

    def poll(self):
        """[('line', text) | ('oversize', length, sha256)] for every line completed since the last poll."""
        try:
            with open(self.path, 'rb') as f:
                f.seek(self.offset)
                data = f.read(CHUNK)
        except FileNotFoundError:
            return []
        self.offset += len(data)
        out = []
        while data:
            nl = data.find(b'\n')
            piece, data = (data, b'') if nl < 0 else (data[:nl], data[nl + 1:])
            if self._over:
                self._len += len(piece)
                self._h.update(piece)
            else:
                self._buf += piece
                if len(self._buf) > self.max_line:
                    self._over, self._len, self._h, self._buf = True, len(self._buf), hashlib.sha256(self._buf), b''
            if nl >= 0:
                if self._over:
                    out.append(('oversize', self._len, self._h.hexdigest()))
                    self._over, self._len, self._h = False, 0, None
                else:
                    if self._buf.strip():
                        out.append(('line', self._buf.decode('utf-8', 'replace')))
                    self._buf = b''
        return out
