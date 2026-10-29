"""The Agent-facing surface of the control path, as a stand-in (1a).  Exactly two tools; neither takes an IA2 variable name,
a command, or a bit pattern.  The real thing (an MCP server or CLI) is a 1b question; this pins down the SHAPE that F1 checks."""
import os
import sys

from standin import Standin
import verifier


class ControlToolset:
    TOOLS = {'ia2_set': ['target_key', 'target', 'ttl_ms'], 'get_action_status': ['action_id']}

    def __init__(self, standin, trace_reader, samples_reader, registry_entries):
        self._standin, self._trace, self._samples, self._entries = standin, trace_reader, samples_reader, registry_entries
        self._requests = {}

    def ia2_set(self, target_key, target, ttl_ms=20000):
        """Ask for the registered target `target_key` to be set to `target`.  Returns the action id."""
        payload = self._standin.submit(target_key, target, ttl_ms=ttl_ms)
        self._requests[payload['action_id']] = payload
        return {'action_id': payload['action_id']}

    def get_action_status(self, action_id):
        """The derived status of an action (read side); never anything the bridge merely claimed."""
        request = self._requests[action_id]
        entry = next(e for e in self._entries if e['key'] == request['target_key'])
        return verifier.derive(request, entry, self._trace(), self._samples(), int(__import__('time').time() * 1000))
