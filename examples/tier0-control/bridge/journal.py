"""Append-only, fsync'd, hash-chained JSONL journal; the bridge's primary record (CONTRACT 8.2).

Everything the bridge knows about past actions (dedup, rate window, single-flight, disabled targets, label use) is rebuilt from
it at start, so a restart forgets nothing.  A torn last line (the process was killed mid-append) is copied aside and cut off;
a broken chain anywhere else refuses to start.  The chain cannot notice a TRUNCATED tail by itself, so every trace event carries
the journal head it was written after (`journal_seq`, `journal_hash`) and the bridge refuses to start on a journal that is
shorter than, or disagrees with, the last anchor in the trace (`check_anchor`).

Action IDs and labels remain occupied for the lifetime of the journal. The horizon only trims
completed action details and rate history; historical terminal events are read by verified file offset."""
import fcntl
import hashlib
import json
import os

from . import durable

GENESIS = '0' * 64
DAY_MS = 24 * 3600 * 1000


class JournalError(Exception):
    pass


class JournalWriteError(JournalError):
    """The journal could not be written or synced.  From then on it takes no more records: whatever the bridge might promise next could not
    be kept, so nothing more is sent.  What is on disk is replayed at the next start."""


class Journal:
    def __init__(self, path, clock_ms, horizon_ms=DAY_MS):
        self.path, self.clock_ms, self.horizon_ms = path, clock_ms, horizon_ms
        self.failed = None         # why the journal stopped taking records (set once, never cleared)
        self.seq, self.last_hash = 0, GENESIS
        self.digests = [bytes.fromhex(GENESIS)]       # digests[n] = hash of record n (digests[0] = genesis)
        self.actions = {}          # action_id -> {'req_hash','target_key','deadline_ms','intent','terminal','response','post_checks','reconciles'}
        self.occupied = {}         # permanent action_id -> request hash and terminal locator
        self._record_offset = 0
        self.disabled = set()
        self.intents = []          # (t_ms, target_key)
        self.labels = {}           # label -> action_id (a label names one action)
        self.quarantined = set()   # sha256 of lines that made the engine raise: never processed again
        self.seen_unattributed = set()   # sha256 of unauthenticated lines / payload hashes / oversize lines already journaled
        self.applied_enables = set()     # sha256 of operator enable lines already handled
        self.torn_copy = None
        self._fh = None
        self._horizon_start = None

    # ---- open / replay ---------------------------------------------------------------------------------------------
    def open(self):
        if self._fh is not None:
            raise JournalError('journal is already open')
        # Lock the actual inode, including when reached through a symlink/hard link.
        # This descriptor owns replay, repair and appends for the whole lifetime.
        fh = open(self.path, 'a+b')
        try:
            try:
                fcntl.flock(fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise JournalError('journal is owned by another bridge') from None
            self._fh = fh
            self._horizon_start = self.clock_ms() - self.horizon_ms
            fh.seek(0)
            data = fh.read()
            good_end = 0
            for raw in data.split(b'\n')[:-1]:
                self._record_offset = good_end
                self._replay_line(raw)
                good_end += len(raw) + 1
            if good_end != len(data):
                self.torn_copy = '%s.torn.%d' % (self.path, self.clock_ms())
                with open(self.torn_copy, 'wb') as f:
                    f.write(data[good_end:])
                fh.truncate(good_end)
                fh.flush()
                durable.sync(fh.fileno())
            # Keep unresolved intents regardless of age. Old completed details can be
            # loaded from the journal without relinquishing their IDs or labels.
            self.actions = {aid: a for aid, a in self.actions.items()
                            if a['terminal'] is None or a['terminal']['t_ms'] >= self._horizon_start
                            or a['terminal'].get('event', {}).get('status') == 'unknown'}
            fh.seek(0, os.SEEK_END)
        except BaseException:
            fh.close()                    # also releases a successfully acquired lock
            self._fh = None
            raise

    def _replay_line(self, raw):
        try:
            rec = json.loads(raw.decode('utf-8'))
        except (ValueError, UnicodeDecodeError):
            raise JournalError('journal line %d is not valid JSON' % (self.seq + 1)) from None
        if rec.get('prev') != self.last_hash or rec.get('seq') != self.seq + 1:
            raise JournalError('journal chain is broken at line %d' % (self.seq + 1))
        self.seq = rec['seq']
        digest = hashlib.sha256(raw).digest()
        self.digests.append(digest)
        self.last_hash = digest.hex()
        self._apply(rec)

    # ---- append ----------------------------------------------------------------------------------------------------
    def append(self, kind, **fields):
        if self.failed:
            raise JournalWriteError('the journal has failed (%s) and takes no more records' % self.failed)
        rec = {'seq': self.seq + 1, 't_ms': self.clock_ms(), 'prev': self.last_hash, 'kind': kind}
        rec.update(fields)
        line = json.dumps(rec, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode('ascii')   # ASCII: no string can fail to encode
        try:
            self._fh.seek(0, os.SEEK_END)
            self._record_offset = self._fh.tell()
            self._fh.write(line + b'\n')
            self._fh.flush()
            durable.sync(self._fh.fileno())
        except (OSError, ValueError) as exc:          # ValueError: the file object is closed
            # The record may or may not have reached the disk, and the in-memory index does not have it.  Take no more records, so that the file
            # can only be a valid chain with or without this last line -- which is what the next start replays.
            self.failed = '%s: %s' % (type(exc).__name__, exc)
            raise JournalWriteError('the journal could not be written or synced (%s)' % self.failed) from exc
        self.seq += 1
        digest = hashlib.sha256(line).digest()
        self.digests.append(digest)
        self.last_hash = digest.hex()
        self._apply(rec)
        return rec

    def close(self):
        if self._fh:
            try:
                self._fh.close()
            except OSError:                           # flushing a journal that could not be written fails again; nothing more can be done
                pass
            self._fh = None

    # ---- anchor ------------------------------------------------------------------------------------------------------
    def head(self):
        return self.seq, self.last_hash

    def check_anchor(self, anchor):
        """`anchor` = (seq, hash) taken from the trace.  None if the journal still contains that exact head, else a reason."""
        if anchor is None:
            return None
        seq, digest = anchor
        if self.seq < seq:
            return 'the journal ends at record %d but the trace says it had reached %d: records were cut off its tail' % (self.seq, seq)
        if self.digests[seq].hex() != digest:
            return 'record %d of the journal is not the one the trace anchored: the history was rewritten' % seq
        return None

    # ---- index -----------------------------------------------------------------------------------------------------
    def _apply(self, rec):
        kind, aid = rec['kind'], rec.get('action_id')
        old = rec['t_ms'] < (self._horizon_start if self._horizon_start is not None else -1)
        if kind == 'intent':
            if aid in self.occupied:
                raise JournalError('action ID has multiple intents: %s' % aid)
            if self.label_in_use(rec['label'], aid):
                raise JournalError('journal label names multiple actions: %s' % rec['label'])
            self.occupied[aid] = {'req_hash': rec['req_hash'], 'terminal': None}
            self.actions[aid] = {'req_hash': rec['req_hash'], 'target_key': rec['target_key'],
                                 'deadline_ms': rec['deadline_ms'], 'intent': rec, 'terminal': None, 'response': None,
                                 'post_checks': [], 'reconciles': []}
            if not old:
                self.intents.append((rec['t_ms'], rec['target_key']))
            self.labels[rec['label']] = aid
        elif kind == 'gate_reject' and aid and aid not in self.occupied:
            self.occupied[aid] = {'req_hash': rec['req_hash'], 'terminal': (self._record_offset, rec['seq'])}
            self.actions[aid] = {'req_hash': rec['req_hash'], 'target_key': rec.get('target_key'),
                                 'deadline_ms': rec.get('deadline_ms'), 'intent': None, 'terminal': rec, 'response': None,
                                 'post_checks': [], 'reconciles': []}
        elif kind == 'response' and aid in self.actions:
            self.actions[aid]['response'] = rec
        elif kind == 'post_check' and aid in self.actions:
            self.actions[aid]['post_checks'].append(rec)       # a restart applies what was recorded right after the write (F08)
        elif kind == 'terminal' and aid in self.actions:
            self.actions[aid]['terminal'] = rec
            self.occupied[aid]['terminal'] = (self._record_offset, rec['seq'])
        elif kind == 'reconcile' and aid in self.actions:
            self.actions[aid]['reconciles'].append(rec)
        elif kind == 'disable':
            self.disabled.add(rec['target_key'])
        elif kind == 'enable':
            self.disabled.discard(rec['target_key'])
            self.applied_enables.add(rec['line_sha256'])
        elif kind == 'enable_ignored':
            self.applied_enables.add(rec['line_sha256'])
        elif kind == 'internal_error':
            self.quarantined.add(rec['line_sha256'])
        elif kind == 'unauthenticated':
            self.seen_unattributed.add(rec['line_sha256'])
        elif kind == 'gate_reject' and not aid:
            self.seen_unattributed.add(rec['req_hash'])

    def has_action(self, action_id):
        return action_id in self.occupied

    def request_hash(self, action_id):
        return self.occupied[action_id]['req_hash']

    def final_event(self, action_id):
        a = self.actions.get(action_id)
        if a is not None:
            return None if a['terminal'] is None else a['terminal'].get('event')
        old = self.occupied.get(action_id)
        if old is None or old['terminal'] is None:
            return None
        offset, seq = old['terminal']
        # Use the locked descriptor and validate the indexed record before reuse.
        self._fh.seek(offset)
        raw = self._fh.readline()
        self._fh.seek(0, os.SEEK_END)
        if not raw.endswith(b'\n') or hashlib.sha256(raw[:-1]).digest() != self.digests[seq]:
            raise JournalError('historical action result no longer matches the journal')
        return json.loads(raw)['event']

    def in_flight_without_terminal(self):
        return [aid for aid, a in self.actions.items() if a['terminal'] is None]

    def label_in_use(self, label, action_id):
        other = self.labels.get(label)
        return other is not None and other != action_id

    def reconcile_due(self, now_ms, recheck_ms):
        """[(action_id, pass_no)]: sent writes that ended `unknown` for a transport/VM reason.  Pass 1 as soon as possible; pass 2
        a while later if the first look found nothing (a write can land after the first look)."""
        out = []
        for aid, a in self.actions.items():
            t = a['terminal']
            if not a.get('intent') or t is None or t['kind'] != 'terminal':
                continue
            ev = t['event']
            if ev['status'] != 'unknown' or ev.get('reason') not in ('runtime_unreachable', 'vm_error', 'bridge_restarted'):
                continue
            seen = a['reconciles']
            if not seen:
                out.append((aid, 1))
            # `landed: false` is how bridges before v0.9 recorded an absent entry.
            elif (len(seen) == 1 and (seen[0].get('audit_entry') == 'absent' or seen[0].get('landed') is False)
                  and now_ms >= seen[0]['t_ms'] + recheck_ms):
                out.append((aid, 2))
        return out

    def open_for_target(self, target_key, now_ms, window_ms=None):
        """Actions on this target the bridge still counts as open: sent but not concluded, or `accepted` and still inside its
        window.  The window ends at the deadline or, if given, `window_ms` after the intent (the read side has decided by then)."""
        out = []
        for aid, a in self.actions.items():
            if a['target_key'] != target_key:
                continue
            t = a['terminal']
            if t is None:
                out.append(aid)
            elif t['kind'] == 'terminal' and t['event']['status'] == 'accepted':
                end = a['deadline_ms'] or 0
                if window_ms is not None and a['intent'] is not None:
                    end = min(end, a['intent']['t_ms'] + window_ms)
                if end > now_ms:
                    out.append(aid)
        return out

    def intent_times(self, since_ms, target_key=None):
        return [t for t, k in self.intents if t >= since_ms and (target_key is None or k == target_key)]
