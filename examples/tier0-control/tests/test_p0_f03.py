"""F03: permanent ID/label occupancy with bounded recent details."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload
from bridge.journal import DAY_MS, Journal, JournalError

class History(unittest.TestCase):
    def test_replay_conflict_and_labels_survive_horizon_and_restarts(self):
        for elapsed in (DAY_MS-1,DAY_MS,DAY_MS+1,10*DAY_MS):
            with self.subTest(elapsed=elapsed), tempfile.TemporaryDirectory() as tmp:
                clock=[1800000000000];client=Client(clock)
                e=engine(tmp,clock,client=client)
                line=envelope(payload(clock[0]));e.handle_line(line)
                original=e.journal.final_event(AID);self.assertEqual(original['status'],'accepted');close(e)
                clock[0]+=elapsed
                # First restart does NOT replay the old request file before reuse.
                for restart in range(3):
                    e=engine(tmp,clock,client=client)
                    e.handle_line(envelope(payload(clock[0],target=2,event_id='new')))
                    self.assertEqual(e.counters['conflicts'],1)
                    e.handle_line(line)
                    self.assertEqual(e.journal.final_event(AID),original)
                    other=AID[:8]+'f'*24
                    e.handle_line(envelope(payload(clock[0],action_id=other)))
                    self.assertEqual(e.journal.final_event(other)['reason'],'conflicting_action_id')
                    self.assertEqual(len(client.posts),1)
                    if elapsed>DAY_MS: self.assertNotIn(AID,e.journal.actions)
                    self.assertTrue(e.journal.has_action(AID));close(e)

    def test_expired_rejection_still_occupies_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock=[1800000000000];client=Client(clock);e=engine(tmp,clock,client=client)
            e.handle_line(envelope(payload(clock[0]-100000)))
            original=e.journal.final_event(AID);self.assertEqual(original['status'],'rejected');close(e)
            clock[0]+=DAY_MS+1;e=engine(tmp,clock,client=client)
            e.handle_line(envelope(payload(clock[0])))
            self.assertEqual(e.journal.final_event(AID),original)
            self.assertEqual(client.posts,[]);close(e)

    def test_old_incomplete_intent_and_response_recover_without_resend(self):
        class Crash(Exception): pass
        for boundary,posts in [('after_intent',0),('after_response',1)]:
            with self.subTest(boundary=boundary),tempfile.TemporaryDirectory() as tmp:
                clock=[1800000000000];client=Client(clock);e=engine(tmp,clock,client=client)
                def stop(): raise Crash()
                e.hooks=type('Hooks',(),{boundary:staticmethod(stop)})()
                with self.assertRaises(Crash): e.handle_line(envelope(payload(clock[0])))
                close(e);clock[0]+=3*DAY_MS;e=engine(tmp,clock,client=client)
                self.assertEqual(e.journal.in_flight_without_terminal(),[AID])
                if posts: self.assertIsNotNone(e.journal.actions[AID]['response'])
                e.recover();e.handle_line(envelope(payload(clock[0],target=2)))
                self.assertEqual(len(client.posts),posts)
                self.assertIsNotNone(e.journal.final_event(AID));close(e)

    def test_ambiguous_legacy_id_or_label_history_refuses_start(self):
        for same_id in (True,False):
            with self.subTest(same_id=same_id),tempfile.TemporaryDirectory() as tmp:
                records=[];prev='0'*64
                for seq,aid in [(1,'a'),(2,'a' if same_id else 'b')]:
                    rec=dict(seq=seq,t_ms=1000,prev=prev,kind='intent',action_id=aid,
                             req_hash='h',target_key='set',deadline_ms=2000,label='same-label',info={})
                    raw=json.dumps(rec,sort_keys=True,separators=(',',':')).encode()
                    records.append(raw);prev=hashlib.sha256(raw).hexdigest()
                path=Path(tmp)/'j';path.write_bytes(b'\n'.join(records)+b'\n')
                with self.assertRaises(JournalError):Journal(str(path),lambda:1000).open()

    def test_historical_locator_checks_integrity_and_chain_failure_is_not_empty_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock=[1800000000000];e=engine(tmp,clock);e.handle_line(envelope(payload(clock[0])));e.journal.append('start');close(e)
            clock[0]+=DAY_MS+1;e=engine(tmp,clock)
            p=Path(tmp)/'journal.jsonl';raw=p.read_bytes()
            p.write_bytes(raw.replace(b'accepted',b'rejected'))
            with self.assertRaises(JournalError): e.journal.final_event(AID)
            close(e)
            with self.assertRaises(JournalError): Journal(str(p),lambda:clock[0]).open()

if __name__=='__main__': unittest.main()
