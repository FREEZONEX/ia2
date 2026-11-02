"""Own one explicitly selected, temporary loopback rumqttd process."""
import json
import os
import selectors
import subprocess


class TempBroker:
    def __init__(self, binary, port, log_path):
        self.binary, self.port, self.log_path = binary, port, log_path
        self.proc = self.logfile = None

    def start(self, timeout=8):
        command = [self.binary, '--bind', '127.0.0.1', '--port', str(self.port)]
        self.logfile = open(self.log_path, 'wb')
        self.proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.logfile)
        with open(self.log_path + '.command.json', 'w') as f:
            json.dump(command, f)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self.proc.stdout, selectors.EVENT_READ)
                if not selector.select(timeout):
                    raise TimeoutError('broker did not report readiness')
                line = os.read(self.proc.stdout.fileno(), 4096)
            with open(self.log_path + '.stdout', 'wb') as f:
                f.write(line)
            if line.strip() != ('READY port=%d' % self.port).encode():
                raise RuntimeError('broker startup failed: %r' % line)
        except BaseException:
            self.stop()
            raise

    def stop(self):
        if self.proc:
            try:
                if self.proc.poll() is None:
                    self.proc.communicate(b'STOP\n', timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.communicate(timeout=3)
            finally:
                for stream in (self.proc.stdin, self.proc.stdout):
                    if stream and not stream.closed:
                        stream.close()
        if self.logfile:
            self.logfile.close()
