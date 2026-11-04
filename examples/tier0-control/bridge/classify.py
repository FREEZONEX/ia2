"""Map the outcome of POST /write onto a status (CONTRACT section 6, M1-M3, M6-M10).  Pure."""

TEXT_403 = 'rejected by project governance'


def _o(state, reason, **extra):
    out = {'state': state, 'reason': reason}
    out.update(extra)
    return out


def classify_write(res, expected_bits, name):
    """`res` is an ia2client result.  Returns {'state','reason', ...}.  A missing key is never read as a null one."""
    if res['kind'] == 'error':
        return _o('unknown', 'runtime_unreachable', detail=res['error_kind'])
    status, text, body = res['status'], res.get('text') or '', res.get('json')
    if status == 200:
        if not isinstance(body, dict) or body.get('ok') is not True or body.get('name') != name \
                or 'undelivered_device' not in body or type(body.get('value')) is not int:
            return _o('unknown', 'vm_error', detail='unexpected 200 body')
        undelivered = body['undelivered_device']
        if undelivered is not None and not isinstance(undelivered, str):
            return _o('unknown', 'vm_error', detail='undelivered_device has an unexpected type')
        applied = body['value']
        if undelivered is not None:
            # Also clamped?  Then the late command will be the clamped value; keep both facts.
            return _o('undelivered', 'link_down', applied_bits=applied, undelivered_device=undelivered,
                      also_modified=(applied != expected_bits))
        if applied != expected_bits:
            return _o('modified', 'clamped', applied_bits=applied)
        return _o('accepted', None, applied_bits=applied)
    if status == 403 and TEXT_403 in text:
        return _o('rejected', 'governance_denied')
    if status == 404 and text.startswith('unknown variable'):
        return _o('rejected', 'unknown_variable', disable_target=True)
    if status == 409 and 'scan loop has stopped' in text:
        return _o('rejected', 'scan_stopped')
    return _o('unknown', 'vm_error', detail='http_%s' % status)
