"""F01: every supported BOOL form obeys registry limits before any POST."""
from pathlib import Path
import sys
import tempfile
import unittest
sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parent))
from p0_support import AID, close, engine, envelope, payload, reg_for
from bridge import values
from bridge.errors import Reject

class BoolRange(unittest.TestCase):
    def test_range_cross_product_and_engine_side_effects(self):
        for lo,hi in [(0,0),(1,1),(0,1)]:
            for value in (False,True,0,1,0.0,1.0):
                allowed=lo<=int(value)<=hi
                with self.subTest(bounds=(lo,hi),value=repr(value)), tempfile.TemporaryDirectory() as tmp:
                    clock=[1800000000000];reg=reg_for(type='BOOL',min=lo,max=hi)
                    e=engine(tmp,clock,reg=reg)
                    e.handle_line(envelope(payload(clock[0],target=value)))
                    ev=e.journal.final_event(AID)
                    self.assertEqual(ev['status'],'accepted' if allowed else 'rejected')
                    self.assertEqual(len(e.client.posts),int(allowed))
                    if allowed: self.assertEqual(e.client.posts[0]['body']['value'],int(value))
                    else:
                        self.assertEqual(ev['reason'],'out_of_range')
                        self.assertEqual(e.journal.intents,[])
                    close(e)

    def test_negative_zero_and_non_bool_values_refused(self):
        for value in (-0.0,2,-1,.5,'0','1',None,[],{},float('inf'),float('nan')):
            with self.subTest(value=repr(value)):
                with self.assertRaises(Reject) as cm:
                    values.validate_and_encode(dict(type='BOOL',min=0,max=1),value)
                self.assertEqual(cm.exception.reason,'bad_value')
        with tempfile.TemporaryDirectory() as tmp:
            clock=[1800000000000];e=engine(tmp,clock,reg=reg_for(type='BOOL',min=0,max=1))
            e.handle_line(envelope(payload(clock[0],target=-0.0)))
            self.assertEqual(e.journal.final_event(AID)['reason'],'bad_value')
            self.assertEqual(e.client.posts,[]);close(e)

if __name__=='__main__': unittest.main()
