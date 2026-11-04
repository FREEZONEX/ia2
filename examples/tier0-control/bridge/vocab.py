"""Status / reason / claim vocabulary, loaded from contract_vocab.json (also read, independently, by lab/verifier.py)."""
import json
import os

_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'contract_vocab.json')
with open(_PATH, encoding='utf-8') as _f:
    VOCAB = json.load(_f)
STATUSES = tuple(VOCAB['statuses'])
REASONS = frozenset(VOCAB['reasons'])
CLAIMS = VOCAB['claims']
