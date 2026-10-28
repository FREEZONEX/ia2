"""F04: required evidence, phase-specific decisions and no resend after missing evidence."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload, reg_for
from bridge import preflight

MISSING=object()
CLEAR={'pending':[],'acknowledged':[],'unconfirmed':[]}
PRE_FIELDS={'/status':['runtime_id','scan_count','mode','mode.kind','forces','fault','latch_reason','watchdog_tripped','device_health'],
            '/health':['status','fault','latch_reason','watchdog_tripped','snapshot_tap','snapshot_tap.running','fieldbus_healthy']}
POST_FIELDS={'/status':['runtime_id','scan_count','fault','latch_reason','watchdog_tripped','last_snapshot','last_snapshot.timestamp_us'],
             '/health':['status','fault','latch_reason','watchdog_tripped','snapshot_tap','snapshot_tap.running']}

def change(doc,path,value):
    if not path: return copy.deepcopy(value)
    parts=path.split('.')
    for part in parts[:-1]: doc=doc[part]
    if value is MISSING: doc.pop(parts[-1],None)
    else: doc[parts[-1]]=copy.deepcopy(value)

class EvidenceClient(Client):
    def __init__(self,clock,edits=(),after=False):
        super().__init__(clock);self.edits=edits;self.after=after;self.force=False
        self.reply_kind='accepted'
    def get_json(self,path):
        body,rec=super().get_json(path)
        if path=='/status': body['last_snapshot']['vars']=[{'name':'sp','type_name':'DINT','bits':0}]
        if not self.after or self.posts or self.force:
            for route,field,value in self.edits:
                if route==path:
                    if not field: body=copy.deepcopy(value)
                    else: change(body,field,value)
        rec['text']=json.dumps(body)
        return body,rec
    def request(self,*args,**kwargs):
        res=super().request(*args,**kwargs)
        if self.reply_kind=='modified': res['json']['value']=0
        elif self.reply_kind=='undelivered': res['json']['undelivered_device']='dev'
        res['text']=json.dumps(res['json']);return res

class Evidence(unittest.TestCase):
    def check_case(self,phase,edits,expected=None):
        with tempfile.TemporaryDirectory() as tmp:
            clock=[1800000000000];client=EvidenceClient(clock,edits,after=phase!='pre')
            e=engine(tmp,clock,client=client);line=envelope(payload(clock[0]))
            if phase=='recovery':
                class Crash(Exception): pass
                def stop(): raise Crash()
                e.hooks=type('Hooks',(),{'after_response':staticmethod(stop)})()
                with self.assertRaises(Crash): e.handle_line(line)
                close(e);e=engine(tmp,clock,client=client);e.recover()
            else: e.safe_handle_line(line)
            ev=e.journal.final_event(AID)
            self.assertEqual(ev['status'],expected or ('rejected' if phase=='pre' else 'unknown'))
            self.assertEqual(len(client.posts),0 if phase=='pre' else 1)
            self.assertEqual(e.counters['internal_errors'],0)
            if expected is None:
                self.assertEqual(ev['reason'],'runtime_unreachable')
                self.assertIn('evidence:',ev['detail'])
            e.handle_line(line);e.recover()
            self.assertEqual(len(client.posts),0 if phase=='pre' else 1)
            close(e);e=engine(tmp,clock,client=client);e.recover();e.handle_line(line)
            self.assertEqual(e.journal.final_event(AID),ev)
            self.assertEqual(len(client.posts),0 if phase=='pre' else 1);close(e)

    def test_original_missing_forces_and_empty_post_health(self):
        for phase,edit in [('pre',('/status','forces',MISSING)),('post',('/health','',{}))]:
            with self.subTest(phase=phase): self.check_case(phase,[edit])

    def test_required_field_deletion_each_phase(self):
        for phase in ('pre','post','recovery'):
            for route,fields in (PRE_FIELDS if phase=='pre' else POST_FIELDS).items():
                for field in fields:
                    with self.subTest(phase=phase,route=route,field=field):
                        self.check_case(phase,[(route,field,MISSING)])

    def test_wrong_types_null_and_empty_objects_each_phase(self):
        common=[('/status','',[]),('/health','',{}),('/health','',[]),('/status','runtime_id',False),
                ('/status','scan_count',True),('/status','watchdog_tripped',0),('/health','watchdog_tripped',None),
                ('/health','fault',False),('/status','latch_reason',False),('/health','snapshot_tap.running',0)]
        for phase in ('pre','post','recovery'):
            extras=([('/status','forces',v) for v in (None,False,{},[{}])]+
                    [('/status','device_health',[{'name':'dev','healthy':1}]),('/status','mode',[])]) if phase=='pre' else [('/status','last_snapshot.timestamp_us',True)]
            for edit in common+extras:
                with self.subTest(phase=phase,edit=edit): self.check_case(phase,[edit])

    def test_optional_failsafe_absent_clear_malformed_and_unmasked(self):
        for phase in ('pre','post','recovery'):
            for route in ('/status','/health'):
                for value in (None,False,{},[],dict(CLEAR,pending=False)):
                    with self.subTest(phase=phase,route=route,value=value):
                        self.check_case(phase,[(route,'failsafe_status',value)])
            for active in ('/status','/health'):
                edits=[(route,'failsafe_status',dict(CLEAR,pending=['dev']) if route==active else CLEAR)
                       for route in ('/status','/health')]
                with self.subTest(phase=phase,active=active):
                    self.check_case(phase,edits,expected='rejected' if phase=='pre' else 'unknown')
        # Positive controls: extension absent (supported binary) and both explicit empty schemas.
        for edits in ([],[(r,'failsafe_status',CLEAR) for r in ('/status','/health')]):
            with tempfile.TemporaryDirectory() as tmp:
                clock=[1800000000000];client=EvidenceClient(clock,edits)
                e=engine(tmp,clock,client=client);e.handle_line(envelope(payload(clock[0])))
                self.assertEqual(e.journal.final_event(AID)['status'],'accepted');self.assertEqual(len(client.posts),1);close(e)

    def test_identity_change_never_accepted(self):
        for phase in ('post','recovery'):
            self.check_case(phase,[('/status','runtime_id','another-runtime')],expected='unknown')

    def test_definite_modified_and_undelivered_are_preserved(self):
        for outcome in ('modified','undelivered'):
            for phase in ('post','recovery'):
                with tempfile.TemporaryDirectory() as tmp:
                    clock=[1800000000000];client=EvidenceClient(clock,[('/health','',{})],after=True)
                    client.reply_kind=outcome;e=engine(tmp,clock,client=client)
                    if phase=='recovery':
                        class Crash(Exception): pass
                        def stop(): raise Crash()
                        e.hooks=type('Hooks',(),{'after_response':staticmethod(stop)})()
                        with self.assertRaises(Crash): e.handle_line(envelope(payload(clock[0])))
                        close(e);e=engine(tmp,clock,client=client);e.recover()
                    else: e.handle_line(envelope(payload(clock[0])))
                    self.assertEqual(e.journal.final_event(AID)['status'],outcome)
                    self.assertEqual(len(client.posts),1);close(e)

    def test_startup_probes_schema_and_healthy_control(self):
        for route,fields in PRE_FIELDS.items():
            for field in fields:
                with self.subTest(route=route,field=field):
                    c=EvidenceClient([1800000000000],[(route,field,MISSING)])
                    result=preflight.check(c,reg_for())
                    self.assertTrue(result['reachable']);self.assertTrue(result['problems']);self.assertEqual(c.posts,[])
        for route,value in [('/status',[]),('/health',{})]:
            self.assertTrue(preflight.check(EvidenceClient([0],[(route,'',value)]),reg_for())['problems'])
        self.assertEqual(preflight.check(EvidenceClient([0]),reg_for())['problems'],[])

if __name__=='__main__': unittest.main()
