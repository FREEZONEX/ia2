"""One exception type for every refusal; `reason` is a code from contract_vocab.json."""


class Reject(Exception):
    def __init__(self, reason, detail=''):
        super().__init__('%s: %s' % (reason, detail))
        self.reason = reason
        self.detail = detail
