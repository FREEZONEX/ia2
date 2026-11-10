# Local regression

Use a separate checkout for each validation run. The full gate needs Python
3.8 or later on macOS or Linux, the Rust toolchain, Node, and the pnpm version
declared in `package.json`. Prepare locked dependencies before going offline.

```sh
git submodule update --init --recursive
cargo fetch --locked
pnpm install --frozen-lockfile
python3 scripts/offline-regression.py
```

`just ci` invokes the same script. The full suite checks agent adaptation,
the runner's own fixtures, rustfmt and Clippy. It builds the debug server
before workspace tests, which generate the Web types and discover the CLI
and storage integration tests. It then builds the four release binaries,
type-checks and builds the Web app, runs its tests, and checks the Git diff.

Use `--suite cli` for the real server and CLI sim tests,
`--suite rust` for workspace tests, and `--suite web` for types, Web
build and Web tests. The `fmt` and `clippy` suites run only their named
check. Prepare dependencies separately for these suites as well. Cargo
runs offline using the selected checkout's target directory by default.
Use separate target directories and checkouts for concurrent runs.
`RUST_TEST_THREADS` can limit test concurrency and appears in the report.

## Evidence

Each run creates a new evidence directory under `target/offline-regression/`.
Use `--evidence-dir` to select another evidence parent outside tracked
sources. The runner prints each command and captures both output streams in
that step's log. `summary.json` records the actual exit codes, UTC+8
timestamps, ignored or filtered test output, checkout identity, tool
versions and dependency lock hashes. Use `--base COMMIT` to record a fixed
comparison base. The diff step checks against this base, or against HEAD
when no base is available. A dirty run records its Git status and tracked
diff hash.

A failed step stops the suite and leaves later steps marked `NOT_RUN`.
SIGINT and SIGTERM stop the active command's process group and preserve the
logs. Steps use the original command's exit status even when the runner
output is redirected. Each invocation runs the selected suite from the start.

## CLI integration server

The tests use a sandbox and a kernel-selected loopback port. They wait for
the child's opt-in `IA2_LISTEN_ADDR` announcement and then probe `/health`
with a deadline. The server comes from the same target and profile as `cs`.
When running Cargo directly, build it first.

```sh
cargo build -p server
cargo test -p ia2-cli --test sim_e2e
```

Per-process launch details and separate stdout and stderr logs appear under
the run's `test-servers/` directory, or next to the debug or release
binaries in `ia2-test-server-logs/` for direct Cargo runs. Each stream
retains up to 128 KiB from its start and 128 KiB from its end. The first
part survives interruption while the end is finalized when the stream
closes. Failed starts identify an unavailable binary, an early exit, or a
readiness deadline and include log tails.
