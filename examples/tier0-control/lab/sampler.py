"""Lab process controls around the independent signed sampler (no unsigned mode)."""
import multiprocessing as mp

from readside.auth import load_keys
from readside.sampler import Collector, decode


class Sampler(mp.Process):
    def __init__(self, port, out_path, keys_path, interval_s=0.2):
        super().__init__(daemon=True)
        self.port, self.out_path, self.keys_path = port, out_path, keys_path
        self._interval = mp.Value('d', interval_s)
        self.stop_flag, self.paused = mp.Event(), mp.Event()
        self._errors = mp.Value('i', 0)

    @property
    def interval_s(self):
        return self._interval.value

    @interval_s.setter
    def interval_s(self, value):
        self._interval.value = value

    @property
    def errors(self):
        return self._errors.value

    def run(self):
        collector = Collector(self.port, self.out_path, load_keys(self.keys_path))
        try:
            while not self.stop_flag.is_set():
                try:
                    if not self.paused.is_set():
                        collector.poll()
                except Exception:
                    self._errors.value += 1
                self.stop_flag.wait(self.interval_s)
        finally:
            collector.close()

    def stop(self):
        self.stop_flag.set()
        self.join(timeout=4)
        if self.is_alive():
            self.terminate()
            self.join(timeout=4)
