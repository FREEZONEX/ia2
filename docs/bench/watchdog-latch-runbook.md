# Bench runbook — scan-watchdog latch (P1 gate)

Verifies on real hardware that a tripped scan watchdog holds outputs at
zero until an explicit restart. Fixture: `examples/watchdog_latch/`.

**Zero motion.** The fixture never commands Operation Enabled. Drives stay
at Ready-to-switch-on throughout; the verdict is read from the statusword.
Nothing on the bench needs to be mechanically prepared.

**Time:** ~40 min on the bench, plus two Linux runtime builds (first
`cross` build can take 10–20 min; later ones are incremental).

**Commits.** The latch fix is commit `34995e7`
(`fix(runtime): make the scan watchdog actually latch outputs off`). The
pre-fix runtime is its parent — compute it, do not guess:

```bash
cd ~/soft-PLC/ia2                       # your checkout of the ia2 repo
PREFIX=$(git rev-parse 34995e7^)        # = 45bc135 at the time of writing
FIXED=$(git rev-parse HEAD)             # 34995e7 or any descendant (this branch's HEAD)
echo "pre-fix=$PREFIX fixed=$FIXED"
```

Once #38 merges, "main after #38" is an equally valid `FIXED`. Do **not**
use `git checkout main` to get the pre-fix tree: the fixture
`examples/watchdog_latch/` is introduced on this branch and disappears
from the working tree the moment you check out `main`. Section 2 builds
the pre-fix runtime from a separate worktree instead.

**Naming.** `edge` below is your `~/.ssh/config` alias for the edge box;
`bench` is the name the edge is registered under inside the fixture
project. `cs ... --edge bench` and `cs deploy bench` take the registered
name, `ssh edge` takes the alias.

---

## 0 · Preconditions

| Check | Command | Expect |
|---|---|---|
| edge up | `ssh edge 'uptime'` | responds (key auth, no password) |
| EtherCAT NIC up | `ssh edge 'ip -br link show enp2s0'` | `UP` — if `DOWN`: `sudo ip link set enp2s0 up` |
| Drives on the chain | `ssh edge 'curl -s localhost:13001/discover'` | slave 0 = `InoSV660N` (the drive the fixture maps), `connected: true`. Extra slaves further down the chain are fine — the fixture's device-level `dc_sync = "sync0"` keeps them from vetoing group OP |
| Rollback point noted | `ssh edge 'readlink -f /opt/ia2/current; sha256sum /opt/ia2/current/runtime'` | note the version dir **and the binary hash** — this is the only way to prove a deploy later changed the binary |
| Edge CPU arch | `ssh edge 'uname -m'` | `x86_64` → build target `x86_64-unknown-linux-gnu`; `aarch64` → `aarch64-unknown-linux-gnu` |
| Dev host can build Linux ELFs | `docker info >/dev/null && echo docker-ok` (for `cross`) **or** `ssh edge 'cargo --version'` (to build on the edge) | one of the two works — see § 2a |
| Dev-host server is NOT already running | `lsof -nP -iTCP:3001 -sTCP:LISTEN` | nothing listed. `IA2_RUNTIME_BIN` is read by the **server** process, so a server started without it will deploy the wrong binary (§ 2b) |

> Nobody else should be using the bench — this restarts `ia2`.

---

## 1 · Baseline: capture the healthy statusword

```bash
ssh edge 'curl -s localhost:13001/status' | python3 -m json.tool | grep -A2 status_word   # name / type_name / value
```

Record the value. Expected `16#1631` = Ready to switch on
(bit0 RTSO, bit4 voltage enabled, bit5 quick-stop inactive, bit6 clear).

**If it is anything else, stop** and resolve that first — the test reads a
transition *from* this state.

---

## 2 · Deploy the fixture with the **pre-fix** runtime

The point of this step is to see the bug. Two things make this non-trivial
and both are silent if you get them wrong:

1. The pre-fix source must come from a **separate worktree** (the fixture
   lives only on this branch).
2. The edge needs a **Linux ELF**. `cargo build` on a macOS dev host
   produces a Mach-O; the server's `find_runtime_binary`
   (`crates/server/src/routes.rs`) refuses non-ELF binaries and `cs deploy`
   then **silently carries the edge's existing `current/runtime` forward**
   (`crates/server/src/edges.rs`, the "Carry forward the runtime binary"
   step of the remote script). The deploy reports success either way.
   `docs/edge-deploy.md` § "One-time edge setup" describes the Linux-binary
   requirement; this runbook pins it down with a hash check.

### 2a · Build both Linux runtime binaries

Source trees:

```bash
cd ~/soft-PLC/ia2
WT=/tmp/ia2-prefix
git worktree add --detach "$WT" "$PREFIX"
# Check out this revision's own compiler pin. Current HEAD uses newer
# upstream APIs and must not be copied into the historical pre-fix tree.
git -C "$WT" submodule update --init --recursive
git -C "$WT" log -1 --oneline            # must print the PREFIX commit, NOT 34995e7
```

The historical fork remains available for old pins; each source tree must
build with the submodule revision recorded by that tree.

Pick the target triple from the `uname -m` check in § 0:

```bash
TARGET=x86_64-unknown-linux-gnu          # or aarch64-unknown-linux-gnu
```

**Option A — `cross` on the dev host (needs Docker).** Same recipe as
`docs/edge-deploy.md`, run once per source tree:

```bash
cargo install cross --git https://github.com/cross-rs/cross   # one-time
rustup target add "$TARGET"                                   # harmless if present

( cd "$WT"           && cross build --release -p ia2-runtime --target "$TARGET" )
( cd ~/soft-PLC/ia2  && cross build --release -p ia2-runtime --target "$TARGET" )

mkdir -p /tmp/ia2-bin
cp "$WT/target/$TARGET/release/ia2-runtime"          /tmp/ia2-bin/ia2-runtime-prefix
cp ~/soft-PLC/ia2/target/$TARGET/release/ia2-runtime /tmp/ia2-bin/ia2-runtime-fixed
```

**Option B — build on the edge (needs a Rust toolchain + crates.io reach
on the edge).** Ship the source, build natively, pull the ELF back:

```bash
mkdir -p /tmp/ia2-bin
for pair in "$WT:prefix" "$HOME/soft-PLC/ia2:fixed"; do
  src=${pair%%:*}; tag=${pair##*:}
  rsync -a --delete --exclude target --exclude node_modules --exclude .git \
        "$src/" "edge:/tmp/ia2-src-$tag/"
  ssh edge "cd /tmp/ia2-src-$tag && cargo build --release -p ia2-runtime"
  scp "edge:/tmp/ia2-src-$tag/target/release/ia2-runtime" "/tmp/ia2-bin/ia2-runtime-$tag"
done
```

(`cargo build --release -p ia2-runtime --manifest-path "$WT/Cargo.toml"` is
the equivalent when the builder can see the worktree directly — e.g. a
Linux dev host — and you would rather not `cd`.)

**Check before touching the edge** — both must be ELF, and they must
differ:

```bash
file /tmp/ia2-bin/ia2-runtime-prefix /tmp/ia2-bin/ia2-runtime-fixed   # "ELF 64-bit LSB ..." both
shasum -a 256 /tmp/ia2-bin/ia2-runtime-prefix /tmp/ia2-bin/ia2-runtime-fixed   # two different hashes
```

Keep these two hashes; § 2b and § 3 compare the edge against them. (The
runtime's self-reported version — `/status.version`, the workspace
`0.0.1` — is identical on every branch and `cs probe`'s `runtime_version`
is never populated, so neither can tell the builds apart.)

### 2b · Start the server with the pre-fix ELF, open the fixture, register the edge

`cs` is a thin client; the deploy (tar + ssh) runs inside the **server**,
and that is the process that reads `IA2_RUNTIME_BIN`. Start it with the
variable in its environment:

```bash
cd ~/soft-PLC/ia2
IA2_RUNTIME_BIN=/tmp/ia2-bin/ia2-runtime-prefix cargo run -p server --release &
# (or: IA2_RUNTIME_BIN=... target/release/server --bind 127.0.0.1:3001 &)
until curl -fs http://127.0.0.1:3001/api/health >/dev/null; do sleep 1; done   # wait for it
```

Point the fixture at the real NIC and open it. The fixture ships with no
edge entry (`project.toml` is just `name`/`version`), so register one after
opening — `cs set edges/<name> --host <ssh-alias>` creates it with the
defaults `install_dir=/opt/ia2`, `runtime_port=13001`, which match
`infra/install.sh`:

```bash
sed -i '' 's/^nic = "_sim".*/nic = "enp2s0"/' examples/watchdog_latch/devices/servo.toml
cs project open examples/watchdog_latch
cs set edges/bench --host edge          # writes examples/watchdog_latch/edges/bench.toml
cs ls edges                             # bench listed
cs probe bench                          # reachable (the bench's normal project is still running)
```

If `cs set edges/bench` complains that it already exists (a previous run
left `edges/bench.toml` behind), just continue.

### 2c · Deploy and prove the binary changed

```bash
ssh edge 'sha256sum /opt/ia2/current/runtime'      # BEFORE — the § 0 hash
cs deploy bench                                    # tar → ssh → versioned extract → symlink swap → restart
ssh edge 'readlink -f /opt/ia2/current; sha256sum /opt/ia2/current/runtime'   # AFTER
```

The AFTER hash must equal `shasum -a 256 /tmp/ia2-bin/ia2-runtime-prefix`.
If it still equals the BEFORE hash, the deploy carried the old binary
forward. Two causes, only one of which logs anything: the server was
started **without** `IA2_RUNTIME_BIN` (it then silently falls back to the
sibling `target/release/ia2-runtime`, a Mach-O on macOS, and ships
nothing — no log line at all), or the variable points at a non-ELF (the
server warns `IA2_RUNTIME_BIN is not a Linux ELF`). The hash is the only
reliable signal. Fix and redeploy; **do not read a verdict off a
carried-forward binary.**

The fixture deploys **healthy** (`burn_n` ships as 0). Arm the overrun
explicitly — this is the moment the test starts:

```bash
cs runtime force burn_n 200000 --edge bench
```

Watch the trip land:

```bash
ssh edge 'journalctl -u ia2 -f | grep -iE "watchdog|overran|failsafe"'
```

Expect within a second or two:

```
scan overran its budget — sliding cadence forward ...
watchdog tripped — engaging failsafe ...
```

**Confirm the overrun is real** (otherwise the whole test is vacuous):

```bash
ssh edge 'curl -s localhost:13001/status' | python3 -c "
import json,sys; d=json.load(sys.stdin)
print('scan_count', d['scan_count'], 'uptime', d['uptime_secs'])
print('scans/s =', round(d['scan_count']/max(d['uptime_secs'],1)))
print('healthy 2ms would be ~500/s; far below that means the burn is working')"
```

If scans/s is still ~500, force a bigger `burn_n` (README explains) — no
redeploy needed.

### Read the verdict — pre-fix

```bash
for i in $(seq 1 10); do
  ssh edge 'curl -s localhost:13001/status' \
    | python3 -c "import json,sys; d=json.load(sys.stdin); \
      v={x['name']:x['value'] for x in d['last_snapshot']['vars']}; \
      print(v.get('status_word'))"
  sleep 1
done
```

(`cs runtime snapshot --vars status_word --edge bench` reads the same
`last_snapshot` through the server if you prefer one hop.)

**Expected pre-fix failure mode:** the statusword either never leaves
`16#1631`, or flicks to Switch-on-disabled and **comes back**. Either way
the zeros did not hold.

Record what you actually see — including "no visible change", which is the
race the code review flagged (the zeros may live on the wire for ~1 frame
and never be sampled).

Note: the pre-fix runtime has no `watchdog_tripped` field in `/health` or
`/status`, so `cs probe bench` prints a plain `✓` on this build. That is
expected and is not the verdict.

---

## 3 · Deploy the **fixed** runtime

The server reads `IA2_RUNTIME_BIN` from its own environment, so swapping
binaries means restarting the server. The edge registration persisted to
`examples/watchdog_latch/edges/bench.toml`, so only the project has to be
re-opened:

```bash
cd ~/soft-PLC/ia2
pkill -f 'target/release/server'         # the server started in § 2b
IA2_RUNTIME_BIN=/tmp/ia2-bin/ia2-runtime-fixed cargo run -p server --release &
until curl -fs http://127.0.0.1:3001/api/health >/dev/null; do sleep 1; done
cs project open examples/watchdog_latch
cs ls edges                              # bench still there

ssh edge 'sha256sum /opt/ia2/current/runtime'      # BEFORE = the prefix hash
cs deploy bench
ssh edge 'readlink -f /opt/ia2/current; sha256sum /opt/ia2/current/runtime'   # AFTER = the fixed hash
cs runtime force burn_n 200000 --edge bench
```

Same rule as § 2c: if the AFTER hash did not move to the
`ia2-runtime-fixed` hash, stop and fix the deploy before reading anything.

Repeat the 10-sample loop from step 2.

### Pass criteria

| # | Criterion | How |
|---|---|---|
| P1 | Watchdog trips | `watchdog tripped` in journal (on this build `cs probe bench` also prints `WATCHDOG LATCHED`) |
| P2 | Statusword leaves Ready-to-switch-on | bit 6 set (Switch on disabled) |
| P3 | **It stays there** — 10/10 samples over ≥10 s | the loop above |
| P4 | Restart clears it | `ssh edge 'sudo systemctl restart ia2'` — the restart drops the force too, so the program comes back healthy: statusword returns to `16#1631` and stays |

P3 is the whole test. P4 proves the latch is a latch and not a permanent
brick.

---

## 4 · Optional — settle the one-frame race

The code review could not determine statically whether the zeros ever
reach the wire before being overwritten (pre-fix), because the scan thread
and the cyclic bus thread are both ~2 ms and unsynchronised.

```bash
ssh edge 'sudo timeout 20 tcpdump -i enp2s0 -w /tmp/wd.pcap ether proto 0x88a4'
scp edge:/tmp/wd.pcap .
```

Look for RxPDO frames whose controlword bytes are `00 00`. Counting them
answers "did the zeros ever go out, and for how many frames".

---

## 5 · Restore the bench

```bash
cd ~/soft-PLC/ia2
cs rm edges/bench                                          # removes examples/watchdog_latch/edges/bench.toml
git checkout -- examples/watchdog_latch/devices/servo.toml # nic back to "_sim"
git status --short examples/watchdog_latch                 # clean
git worktree remove /tmp/ia2-prefix                        # drop the pre-fix source tree
pkill -f 'target/release/server'                           # server with IA2_RUNTIME_BIN set — don't leave it running

# put the bench's normal project back (start the server WITHOUT IA2_RUNTIME_BIN;
# the deploy then carries the fixed runtime forward, which is what you want)
cargo run -p server --release &
until curl -fs http://127.0.0.1:3001/api/health >/dev/null; do sleep 1; done
cs project open <dir of whatever the bench normally runs>
cs deploy <that project's edge name>
ssh edge 'curl -s localhost:13001/status | head -c 200'    # normal project back
```

If you instead want the exact pre-test binary back, point `current` at the
version dir noted in § 0 and restart `ia2` (rollback is a symlink swap —
see `docs/edge-deploy.md` § "Layout on the edge").

Leaving the lab: shut the edge down the usual way (`ssh -t edge 'sudo shutdown -h now'`).

---

## Recording the result

Whatever happens, write the observed statusword sequences for both builds
into the PR description, **together with the three sha256 hashes** (§ 0
baseline, after the pre-fix deploy, after the fixed deploy) — they are the
evidence that the two runs actually exercised two different binaries. A
"we saw no change pre-fix" result is still a result — it means the failure
mode is the race rather than the rewrite, and the latch is still the
correct fix (it removes both).
