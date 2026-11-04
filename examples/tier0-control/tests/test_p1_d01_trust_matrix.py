"""P1 D01: the D11 design must not claim that two signing keys make a wrong `verified` need two failures.

Each signer (the bridge for events, the sampler for samples) is a TRUST POINT for the correctness of what it reports: a valid signature says who
said it and that it was not changed, not that it is true.  So a wrong `verified` needs only ONE wrong signer plus a real, matching record from
the other end -- and the read side cannot tell the difference.

This is a design check: it reads the design document (override the path with P1_DESIGN_D11 to check another copy), and it shows the limitation
the document states on the REAL verifier, using booleans for "authenticated" and "facts correct" -- fixture records are now signed before entering the real verifier. D11 authentication itself is tested in test_d11_auth.py."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path
import itertools
import os
import re
import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
SRC = HERE.parent
sys.path[:0] = [str(HERE), str(SRC / 'lab')]
import test_verifier as tv  # noqa: E402
import signing_fixtures as verifier  # noqa: E402

DOC = Path(os.environ.get('P1_DESIGN_D11') or SRC / 'contract' / 'DESIGN-D11-signed-events.md')


def section(text, number):
    m = re.search(r'^## %d .*?(?=^## \d|\Z)' % number, text, re.S | re.M)
    return m.group(0) if m else ''


class Document(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = DOC.read_text(encoding='utf-8')
        cls.s1, cls.s7 = section(cls.text, 1), section(cls.text, 7)

    def test_the_false_claim_is_gone(self):
        for claim in ('需要偷到桥的事件密钥**和**采样器的密钥', '两个都签，伪造 `verified` 需要'):
            self.assertNotIn(claim, self.text, 'the design still says a wrong verified needs both keys')
        self.assertNotIn('还需要桥的真实 `accepted` 事件（或桥密钥）才能凑出', self.s7, 'a compromised sampler needs no bridge key')

    def test_section_1_says_what_signatures_do_and_do_not_do(self):
        for piece in ('事件与样本分别认证来源，并依赖各自签名者正确报告事实',
                      '任一事实来源出错，结合另一端真实且匹配的记录，也可能形成错误 `verified`',
                      '独立密钥不是双人授权', '`verified` 仍不证明物理效果', '正确性信任点', '签名有效'):
            self.assertIn(piece, self.s1)

    def test_section_7_has_the_trust_matrix_with_every_case(self):
        rows = [l for l in self.s7.splitlines() if l.startswith('|') and not l.startswith('|---') and '事件：认证' not in l]
        self.assertEqual(len(rows), 5, 'both right, sample wrong, event wrong, both wrong, and the unauthenticated case')
        verdicts = [r.strip('|').split('|')[-1].strip() for r in rows]
        trusted = [('可信' in v and '不可信' not in v) for v in verdicts]
        self.assertEqual(trusted, [True, False, False, False, False], verdicts)
        self.assertIn('没有 `verified`', verdicts[4])
        for r in rows[1:4]:
            self.assertIn('无法识别', r)

    def test_section_7_names_the_limits_it_does_not_promise_to_remove(self):
        for piece in ('识别不了', '独立密钥不是双人授权', '不作保证的事', '签名有效但事实错误', '两把密钥互相制衡'):
            self.assertIn(piece, self.s7)

    def test_d11_is_implemented_without_changing_the_trust_limit(self):
        self.assertIn('本地实现状态', self.text)
        from readside.verifier import derive
        with self.assertRaises(TypeError):
            derive(tv.REQ, tv.ENTRY, [], [], tv.AS_OF)


class Matrix(unittest.TestCase):
    """The four 'authenticated' rows of the matrix, on the real verifier.  Every fixture is correctly signed; the truth of a signed report is still outside the verifier's knowledge."""

    @staticmethod
    def received(event_true, sample_true):
        """The records the read side gets.  Whether the facts in them are true is NOT in them -- a bridge that reports an `accepted` write that never
        happened, or a sampler that reports a feedback the device does not have, produces exactly the records an honest one does."""
        return [dict(tv.ACC, requested_value=12.5)], [tv.sample(t=6_000_000, value=12.5)]

    def test_the_read_side_cannot_tell_a_valid_false_report_from_a_true_one(self):
        verdicts = set()
        records = []
        for event_true, sample_true in itertools.product((True, False), repeat=2):
            events, samples = self.received(event_true, sample_true)
            records.append((events, samples))
            verdicts.add(verifier.derive(tv.REQ, tv.ENTRY, events, samples, tv.AS_OF)['status'])
        self.assertTrue(all(r == records[0] for r in records), 'the records do not depend on the truth of their facts')
        self.assertEqual(verdicts, {'verified'}, 'so the verdict cannot depend on it either: a valid-but-false report is verified like a true one')

    def test_a_wrong_verified_needs_one_wrong_signer_not_two(self):
        # the design's claim, as the table: with both ends authenticated, a verified is trustworthy only if BOTH facts are correct
        for event_auth, event_true, sample_auth, sample_true in itertools.product((True, False), repeat=4):
            usable = event_auth and sample_auth
            trustworthy = usable and event_true and sample_true
            wrong = usable and not (event_true and sample_true)
            self.assertEqual(wrong, usable and (not event_true or not sample_true))
            if event_true != sample_true:
                self.assertTrue(wrong or not usable, 'one wrong signer is enough')
            self.assertFalse(trustworthy and wrong)


if __name__ == '__main__':
    unittest.main()
