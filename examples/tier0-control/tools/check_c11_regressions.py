"""Real-process C11 regressions: runtime link loss and broker port ownership.

All listeners are loopback and owned by this runner. Existing evidence is never
overwritten. Build the four explicitly selected binaries before running.
"""
import argparse
import asyncio
import json
from pathlib import Path
import socket
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'lab'))
import harness
import cases_c11
from minimqtt import MiniMqttClient
from test_broker import TempBroker


def save(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')


class RuntimeLink:
    def __init__(self):
        self.writers, self.tasks, self.events = set(), set(), []
        self.blocked = False

    def record(self, event):
        self.events.append(dict(event=event, time_utc8=cases_c11.utc8()))

    async def pipe(self, reader, writer):
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
        except (OSError, asyncio.CancelledError):
            pass

    async def connection(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        self.writers.add(writer)
        upstream = None
        try:
            if self.blocked:
                self.record('reconnect_refused')
                return
            upstream_reader, upstream = await asyncio.open_connection('127.0.0.1', harness.MQTT_PORT)
            self.writers.add(upstream)
            self.record('runtime_mqtt_connected')
            pumps = [asyncio.create_task(self.pipe(reader, upstream)),
                     asyncio.create_task(self.pipe(upstream_reader, writer))]
            _, pending = await asyncio.wait(pumps, return_when=asyncio.FIRST_COMPLETED)
            for pending_task in pending:
                pending_task.cancel()
            await asyncio.gather(*pumps, return_exceptions=True)
        except OSError:
            self.record('connect_failed')
        finally:
            for stream in (writer, upstream):
                if stream is not None:
                    stream.close()
                    try:
                        await stream.wait_closed()
                    except OSError:
                        pass
                    self.writers.discard(stream)
            self.tasks.discard(task)

    async def trip(self, directory):
        while not (directory / 'drop-request').exists():
            await asyncio.sleep(0.005)
        self.blocked = True
        self.record('runtime_mqtt_cut_before_disabled_publish')
        for writer in list(self.writers):
            writer.transport.abort()
        await asyncio.sleep(0.05)
        save(directory / 'drop-done.json', self.events[-1])


async def check_dropout(args, label, allow, cut):
    directory = Path(args.out) / label
    directory.mkdir()
    original_northbound, original_file = cases_c11.northbound, cases_c11.__file__
    proxy_port = harness.MQTT_PORT + 1
    link = RuntimeLink()
    server = await asyncio.start_server(link.connection, '127.0.0.1', proxy_port)
    trigger = asyncio.create_task(link.trip(directory / 'C11-r1')) if cut else None
    try:
        cases_c11.northbound = lambda prefix, enabled: original_northbound(prefix, enabled or allow).replace(
            'broker_port = %d' % harness.MQTT_PORT, 'broker_port = %d' % proxy_port)
        if cut:
            cases_c11.__file__ = str(ROOT / 'tools/c11_dropout_worker.py')
        harness.CASES.update(cases_c11.CASES)
        case_args = argparse.Namespace(**vars(args))
        case_args.out = str(directory)
        verdict = await harness.run_case('C11', case_args)
        failed = [c['name'] for c in verdict['checks'] if not c['ok']]
        expected = ('disabled runtime MQTT stays live throughout observation' if cut else
                    'disabled write produces no audit during 2 second window')
        passed = (verdict['verdict'] == 'FAIL' and verdict['error'] is None and expected in failed
                  and (not cut or link.blocked))
        result = dict(test=label, passed=passed, expected='FAIL with ' + expected,
                      actual=verdict['verdict'], failed_checks=failed, error=verdict['error'],
                      allow_write_in_disabled_stage=allow, cut_runtime_link=cut, proxy_events=link.events)
    finally:
        cases_c11.northbound, cases_c11.__file__ = original_northbound, original_file
        if trigger:
            trigger.cancel()
            await asyncio.gather(trigger, return_exceptions=True)
        server.close()
        await server.wait_closed()
        for writer in list(link.writers):
            writer.transport.abort()
        await asyncio.gather(*list(link.tasks), return_exceptions=True)
    result['proxy_port_free'] = harness.port_free(proxy_port)
    result['passed'] &= result['proxy_port_free']
    save(directory / 'regression.json', result)
    return result


def refused_broker(binary, port, directory):
    broker = TempBroker(binary, port, str(directory / 'broker.log'))
    error = None
    try:
        broker.start()
    except (RuntimeError, TimeoutError) as exc:
        error = repr(exc)
    finally:
        broker.stop()
    stdout = directory / 'broker.log.stdout'
    output = stdout.read_text() if stdout.exists() else ''
    return dict(passed=error is not None and broker.proc.returncode != 0 and 'READY' not in output,
                error=error, exit=broker.proc.returncode, stdout=output,
                stderr=(directory / 'broker.log').read_text())


def check_brokers(args):
    directory = Path(args.out) / 'broker-ownership'
    directory.mkdir()
    results = []
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        result = refused_broker(args.broker_bin, listener.getsockname()[1], directory)
        result['test'] = 'occupied_plain_tcp_listener'
        results.append(result)
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        port = reserve.getsockname()[1]
    owner = TempBroker(args.broker_bin, port, str(directory / 'owner.log'))
    client = MiniMqttClient('127.0.0.1', port, 'c11-regression-owner')
    try:
        owner.start()
        duplicate = directory / 'duplicate'
        duplicate.mkdir()
        result = refused_broker(args.broker_bin, port, duplicate)
        result['test'] = 'occupied_real_mqtt_broker'
        results.append(result)
        client.connect()
        client.subscribe('regression/#', qos=1)
        client.publish('regression/ping', 'still-owned', qos=1)
        client.wait_for_message('regression/ping', lambda m: m.payload == b'still-owned')
        client.ping()
    finally:
        client.disconnect()
        owner.stop()
    results.append(dict(test='owner_remains_functional_and_stops',
                        passed=owner.proc.returncode == 0 and harness.port_free(port), exit=owner.proc.returncode))
    save(directory / 'regression.json', results)
    return results


async def main(args):
    results = check_brokers(args)
    for label, allow, cut in [('dropout-disabled', False, True), ('dropout-enabled', True, True),
                              ('wrong-allow-control', True, False)]:
        results.append(await check_dropout(args, label, allow, cut))
    summary = dict(time_utc8=cases_c11.utc8(), passed=all(r['passed'] for r in results), tests=results,
                   binaries={name: harness.sha256_file(getattr(args, name))
                             for name in ('bin', 'broker_bin', 'server_bin', 'cs_bin')})
    save(Path(args.out) / 'summary.json', summary)
    print(json.dumps(summary, indent=2))
    return 0 if summary['passed'] else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('bin', 'broker-bin', 'server-bin', 'cs-bin', 'out'):
        parser.add_argument('--' + flag, required=True)
    args = parser.parse_args()
    for key, value in vars(args).items():
        setattr(args, key, str(Path(value).resolve()))
    output = Path(args.out)
    if not output.is_relative_to(ROOT / 'evidence') or output.exists():
        parser.error('--out must be a fresh directory inside examples/tier0-control/evidence')
    output.mkdir(parents=True)
    raise SystemExit(asyncio.run(main(args)))
