"""Trace output toward the Tier0 stand-in: one JSON line per event, idempotent by event_id.  Every line can carry the journal head
it was written after (`journal_seq`, `journal_hash`): an anchor kept OUTSIDE the journal, so a journal cut short is noticed."""
import json
import os
import tempfile

from . import durable, eventauth


def _record_of(line, keys, revoked):
    """(event_id, anchor) of a complete line this bridge could have written, else None.  The bridge writes ASCII JSON objects with a
    string event ID, so any other complete line (foreign or damaged) is skipped the same way, whatever it fails on."""
    try:
        rec = eventauth.verify(line.decode('ascii'), keys, revoked)
    except (ValueError, RecursionError):
        return None
    if not isinstance(rec, dict) or not isinstance(rec.get('event_id'), str):
        return None
    seq, digest = rec.get('journal_seq'), rec.get('journal_hash')
    anchor = (seq, digest) if type(seq) is int and seq >= 0 and isinstance(digest, str) else None
    return rec['event_id'], anchor


class TraceWriter:
    def __init__(self, path, event_keys, revoked=()):
        if not event_keys:
            raise ValueError('event keys are required')
        self.event_keys, self.revoked = event_keys, frozenset(revoked)
        self.path = path
        self.seen = set()
        self.last_anchor = None            # (journal_seq, journal_hash) with the highest seq found in the file
        self._dirty_offset = None
        self.torn_copy = None
        self.skipped = []                  # 1-based numbers of complete lines that are not bridge records
        # Unbuffered bytes make short writes and failed flushes recoverable at a
        # known file offset; no hidden text buffer can append an old failed line.
        self._fh = open(path, 'a+b', buffering=0)
        try:
            self._fh.seek(0)
            number = 0
            while True:
                offset = self._fh.tell()
                line = self._fh.readline()
                if not line:
                    break
                number += 1
                if not line.endswith(b'\n'):
                    with tempfile.NamedTemporaryFile(prefix=os.path.basename(path) + '.torn.',
                            dir=os.path.dirname(os.path.abspath(path)), delete=False) as backup:
                        self.torn_copy = backup.name
                        backup.write(line); backup.flush(); durable.sync(backup.fileno())
                    self._fh.truncate(offset)
                    break
                else:
                    record = _record_of(line, self.event_keys, self.revoked)
                    if record is None:
                        self.skipped.append(number)
                    else:
                        self.seen.add(record[0])
                        anchor = record[1]
                        if anchor is not None and (self.last_anchor is None or anchor[0] > self.last_anchor[0]):
                            self.last_anchor = anchor
            # A complete line may have survived a previous sync failure. Confirm
            # it now before treating it as published and suppressing a retry.
            durable.sync(self._fh.fileno())
            self._fh.seek(0, os.SEEK_END)
        except BaseException:
            self._fh.close()
            raise

    def close(self):
        self._fh.close()

    def emit(self, event, anchor=None):
        if event['event_id'] in self.seen:
            return False
        if anchor is not None:
            event = dict(event, journal_seq=anchor[0], journal_hash=anchor[1])
        envelope = eventauth.sign(event, self.event_keys, self.revoked)
        data = (json.dumps(envelope, sort_keys=True, separators=(',', ':'), ensure_ascii=True) + '\n').encode('ascii')
        if self._dirty_offset is not None:
            # The previous append may be empty, partial or complete but unsynced.
            # Remove only that uncommitted suffix before any new append.
            self._fh.truncate(self._dirty_offset)
            durable.sync(self._fh.fileno())
            self._dirty_offset = None
        self._fh.seek(0, os.SEEK_END)
        self._dirty_offset = self._fh.tell()
        remaining = memoryview(data)
        while remaining:
            written = self._fh.write(remaining)
            if not written:
                raise OSError('trace write made no progress')
            remaining = remaining[written:]
        durable.sync(self._fh.fileno())
        self.seen.add(event['event_id'])
        self._dirty_offset = None
        return True
