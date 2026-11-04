"""'lab' builds may load a hooks module (--test-hooks); 'prod' builds must not contain that code at all.  tools/package_prod.py
rewrites this file and strips the lab-only blocks."""
FLAVOR = 'lab'
