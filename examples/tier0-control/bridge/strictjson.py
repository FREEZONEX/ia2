"""json.loads that refuses duplicate keys and NaN/Infinity (both parse silently in the stdlib)."""
import json


class StrictJSONError(ValueError):
    pass


def _pairs(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise StrictJSONError('duplicate key %r' % key)
        out[key] = value
    return out


def _constant(name):
    raise StrictJSONError('non-finite constant %s' % name)


def loads(text):
    try:
        return json.loads(text, object_pairs_hook=_pairs, parse_constant=_constant)
    except StrictJSONError:
        raise
    except (ValueError, RecursionError) as exc:
        raise StrictJSONError(str(exc)) from None
