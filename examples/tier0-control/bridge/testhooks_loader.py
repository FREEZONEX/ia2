"""LAB ONLY: loads a hooks module from a file path.  Not part of a production package (tools/package_prod.py leaves it out)."""
import hashlib
import importlib.util


def load_hooks(path):
    spec = importlib.util.spec_from_file_location('bridge_testhooks', path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    with open(path, 'rb') as f:
        return mod.HOOKS, hashlib.sha256(f.read()).hexdigest()
