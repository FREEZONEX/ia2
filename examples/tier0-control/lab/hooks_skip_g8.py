"""Lab-only bridge hooks module (never shipped): switches off gate G8 so the bridge writes into a latched runtime and
the RUNTIME's behaviour can be observed (C1 b).  Everything else is a no-op."""


class _Hooks:
    def skip_gate(self, name):
        return name == 'G8'

    def after_gates(self):
        pass

    def after_intent(self):
        pass

    def after_http(self):
        pass

    def after_response(self):
        pass


HOOKS = _Hooks()
