# R3 时序覆盖与对账语义修复交接报告

日期：2026-10-29 UTC+8。上一轮（IR03、`failsafe_status: null` 兼容、D6）的交接内容见本文件的 git 历史（`2d68f5b`）。

本轮按上一轮的待办做了 **R3 时序覆盖**，结果是一个新 FINDING：runtime 已读入并排队的写入，在客户端断开后**照样执行**，却**不记 `/audit`**。桥原先把 `/audit` 查不到记成 `landed: false`，与事实不符。本轮已改正桥的对账语义、契约与断言并重跑；之后又修了 `trace.py` 启动读取的不一致，并在**另一个分支**修了 IA2 运行时本身（见下文「运行时修复」）。最终代码上完整回环联调 **31 个用例全部 PASS**（471 条断言无失败），R3 组 5 个用例各 3 遍 **15/15 PASS**；R3 原来的 FINDING 按实测改写 EXPECT 后关闭。

[PR #99](https://github.com/FREEZONEX/ia2/pull/99) 已于 2026-10-29 由维护者合并（`a2dec2a`），合并前维护者补了一个提交 `f44755a`：harness 改用 `subprocess.Popen` 管理 runtime、在线程里等它退出，避免 macOS 上 Python 3.14.7 的 asyncio 子进程监视器在 `SIGSTOP` 时卡住事件循环。**本轮修改在合并后的 `main` 上另起分支 `fix/tier0-r3-audit-reconcile`，未推送、没开 PR。** 运行时修复同样未推送、没开 PR。

## 交付定位

| 项目 | 内容 |
|---|---|
| 仓库 / 目标分支 | `FREEZONEX/ia2` / `main` |
| PR 分支 / 头 | `codex/tier0-ir03-publication-recovery` / `2d68f5b6976d35448084a6c922dcd51a83e4ca3c`（本轮改动在工作树，未提交） |
| 对账修复 | [bridge/engine.py](bridge/engine.py) `_reconcile_one`、[bridge/journal.py](bridge/journal.py) `reconcile_due`、[lab/verifier.py](lab/verifier.py) |
| 时序用例 | [lab/cases_p0.py](lab/cases_p0.py) R3、R3T1、R3T1K、R3T1X、R3T2；[lab/recproxy.py](lab/recproxy.py) 的两个开关 |
| 回归 | [tests/test_r3_reconcile.py](tests/test_r3_reconcile.py)（9 项）；[tests/test_trace_reload.py](tests/test_trace_reload.py)（4 项）；`tests/test_fixes.py` W9 断言更新 |
| trace 启动读取 | [bridge/trace.py](bridge/trace.py) `_record_of`、[bridge/main.py](bridge/main.py) 的 stderr 提示 |
| 契约 | [CONTRACT.md](contract/CONTRACT.md) v0.9（M10、§8.3、§11.5、§12）；[TEST-CASES.md](contract/TEST-CASES.md) v0.8；[DESIGN-1A.md](contract/DESIGN-1A.md) v0.7 |
| 验证详情 / 机器可读结果 | [VALIDATION.md](VALIDATION.md)、[validation-summary.json](validation-summary.json) |

## R3 时序结果（实测，3/3）

| 用例 | runtime 卡在哪里 | 执行了吗 | `/audit` | 桥最终对账（修复后） |
|---|---|---|---|---|
| R3 | 还没读到请求（`Recv-Q` > 0） | 否 | 无 | `absent`、`landed: null` |
| R3T1 | 已读完并排队，扫描循环还没执行（1 s 注入停顿中 `SIGSTOP`） | **是**（VM + 设备） | **无** | `absent`、`landed: null` |
| R3T1K | 同 R3T1，但 runtime 一侧连接保持打开（对照） | 是 | 有 | `found`、`landed: true` |
| R3T1X | 同 R3T1，改由桥 `SIGKILL` 触发 | **是** | **无** | `unknown / bridge_restarted`；`absent`、`landed: null` |
| R3T2 | 已执行并应答，响应没送到桥 | 是 | 有 | `found`、`landed: true` |

原因（读码，R3T1K 对照证实）：`crates/runtime/src/main.rs` 的 `rt_write` 在 `write_with_pulse(...).await` 之后才写审计；`send_write` 先把命令放进通道再等应答；hyper 1.9 默认 `h1_half_close = false`，忙时读到 EOF 就丢弃处理函数。`rt_force`、`rt_unforce` 同样写法（未运行）。这是 IA2 运行时缺陷，与 `rt_audit` 文档不符，**本 PR 不改 Rust**。

## 修复内容

- **对账记录**：`reconcile_note` 与 journal 的 `reconcile` 记录新增 `audit_entry`（`found` / `absent` / `unavailable`）；`landed` 只在 `found` 时为 `true`，其余为 `null`，**不再写 `false`**。`detail` 写明"无条目不证明没有执行"及原因。两次对账的节奏、`final` 的含义、状态不升级、不重发均不变。
- **兼容**：旧桥写的 `landed: false` 第一次对账记录仍会触发第二次对账；读侧把旧 `landed: false` 读成 `absent`、`landed: null`；畸形字段不构成任何断言。
- **恢复**：崩溃恢复找不到条目时状态仍为 `unknown / bridge_restarted`（或 `runtime_restarted`），`detail` 注明"不证明没有执行"。
- **断言**：R1、B11 的"最终 `landed: false`"改为 `absent` / `null`；R3 的 EXPECT 按 3 遍实测改写（请求未读走时不执行），并加 `Recv-Q`、VM、设备日志核对；新增 4 个 P0 时序用例。按 TEST-CASES §2 在变更记录登记"运行后改 EXPECT"。历史 FINDING 留证不动。R3T1X 的桥侧断言跟随重启时 `/audit` 的实际内容（有条目则按 D8 收尾为 `accepted`、读侧 `verified`），所以换上修好的 runtime 时只报 FINDING，不误报 FAIL。
- **trace 启动读取**：原先已有 trace 里含非 ASCII 字节的行、或不是 JSON 对象的 JSON 行（如 `[1]`），会让桥以未捕获异常结束启动（不是 §8.4 的任何退出码），而非 JSON 的行却被静默跳过。现在凡是桥不可能写出的完整行（非 ASCII、不是对象、事件 ID 不是字符串、嵌套过深）一律跳过、不据其去重或取锚，并在 stderr 报出行号；锚只认非负整数 `journal_seq` 加字符串 `journal_hash`。CONTRACT §8.4 的 trace 段与 README 已同步。修复前新测试 4 项全部报错或失败（进程测试里桥打印回溯后退出），留证 `evidence/trace-reload-before/`。

## 运行时修复（另一分支，未提交）

`/audit` 漏记的根因在 IA2 运行时，按待办单独修，没有动 PR #99 的分支：

| 项目 | 内容 |
|---|---|
| 位置 | worktree `~/Projects/soft-PLC/ia2-control-write-detached-20261029`，分支 `fix/control-write-detached`，自 `origin/main` `e6a38aa`（`--no-track`），**未提交** |
| 读码新发现（更严重） | `monitor::write_with_pulse` 在调用方的 future 里等 `start_pulse`：调用方在脉冲写入排队后被丢弃时，写入照样落地，但复位任务从未建立，**值一直保持**——正是 `pulse_ms` 要防的事。IDE 服务与边缘 runtime 共用这段代码，两边都受影响 |
| 修复 | 脉冲写入与复位放进同一个独立任务；runtime 的 `/write`、`/force`、`/unforce` 把命令与审计放进独立任务（`run_detached`：handler panic 仍交给 CatchPanicLayer，运行时关停时答 503） |
| 回归 | `pulse_reset_survives_a_caller_dropped_while_the_write_is_queued`（bridge 库）、`control_writes_dropped_while_queued_are_still_audited`（runtime）；修复前分别得到"值停在 1"与"`/audit` 为空而 x=7"，修复后通过，各重复 15 次全过 |
| 文档 | `docs/api.md`（脉冲约定、`/audit` 行）、技能 02/03 |
| 仓库检查 | 适配、格式、Clippy、server 构建、发布构建、Web 构建、Web 423 项均过；Rust 全量 `--no-fail-fast` **8591 通过 / 0 失败 / 105 忽略**（检查脚本里第一次 `cargo test` 在 `sim_e2e` 的一个用例上报"server 5 秒内没起来"并就此停下，重跑全量未复现） |
| 端到端 | 修好的 runtime（`b7e455d4…`）上 R3 组 ×3：R3、R3T1K、R3T2 PASS；**R3T1、R3T1X 均为 FINDING（执行且有审计条目），0 条断言失败**——正是预期：合并后要把这两行的 `R3_EXPECT` 改成 `(True, True)` 并更新 CONTRACT §11.5 |
| 报告 | 仓库外 `~/Projects/soft-PLC/pr99-r3-timing-20261029/RESULT-RUNTIME-FIX.md` |

## 验收记录

| 验证 | 结果与范围 |
|---|---|
| Python 单元与进程测试（最终代码） | **266 项通过**，含新增 9 项 R3 对账回归、4 项 trace 启动读取回归 |
| 新回归套用未修复的代码 | R3 对账 9/9 检出（`2d68f5b`：2 个断言失败、7 个错误，`evidence/r3-reconcile-before/`）；trace 4/4 检出（`evidence/trace-reload-before/`） |
| 完整回环联调 31 例（`main` 的 runtime `52160ab8…`） | **31 PASS**，471 条断言 0 失败，无启动错误；全部 verdict 的代码树哈希同为 `a0311eb5e8adf1e9877276e7a645f8d0303aca662350a26ea87eccabb56d2e46`（即当前工作树） |
| R3 组 ×3 遍（同上） | 15/15 PASS（261 条断言）；真实情况每次都与 EXPECT 一致；R3T1、R3T1X 共 6 次都复现 IA2 漏记审计 |
| R3 组 ×3 遍（修好的 runtime `b7e455d4…`，同一代码树） | 9 PASS + 6 FINDING（R3T1、R3T1X 执行且有审计），258 条断言 0 失败；留证仓库外 `pr99-r3-timing-20261029/runs/detached-runtime-2/` |
| 逐动作崩溃恢复检查 | 3 个动作通过 |
| BOOL 假设备验收 | 21 项通过（`evidence/bool-device-6/`） |
| nullable-failsafe 变异对照 | 3/3 检出 |
| 仓库级检查（PR #99 工作树） | 本轮早些时候 8 步全过（Rust 8589 / 105 忽略，Web 423）；之后只改了 Python 与文档，未重跑。发布构建重新链接了 `target/release/ia2-runtime`（`54df4183…`，留作 `evidence/gate-r3fix-20261029/ia2-runtime.gate-build`），之后已把联调用的 `52160ab8…` 放回原路径 |
| `cs` 无设备模拟 | 本轮未重跑（PR #99 的 Rust、工程、场景均未改） |

运行时二进制仍为 `52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de`（构建标识 `047e7c17ffda`，源码同 `main` `e6a38aa`）。本轮有一次联调中途被我有意停在 C4（运行中改了实验室文件），部分留证在 `evidence/r3fix-20261029-e2e-aborted/`，不计入结果。`evidence/r3fix-20261029-final/`（代码树 `d5ed5fba…`）是 trace 修复与 R3T1X 判据调整前的一次全过运行，已被 `evidence/r3fix-20261029-final2/`（含 `summary.json`）取代。修复前的探索运行与修好 runtime 上的运行在仓库外 `~/Projects/soft-PLC/pr99-r3-timing-20261029/`。

## 复跑

仓库根目录，Python 3.8+，无需安装依赖：

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py
python3 -I -B examples/tier0-control/tools/check_failsafe_controls.py
```

完整联调（含 R3 时序用例）见 [README.md](README.md)；用例共用固定回环端口，须串行，每次用新的证据目录。R3 时序用例在 macOS 上记录（`netstat -an -p tcp`），Linux 的 `netstat -tan` 形式未测。

## 验收待办

- [x] **R3 时序覆盖**：读取前、已读并排队、已执行未应答三种时序，以及连接保持打开的对照和桥崩溃变体，各 3 遍。
- [x] **R3 判据与复跑**：EXPECT 按实测改写，对账语义与契约、断言同步，受影响用例重跑（结果见上）。
- [x] **IA2 运行时审计修复（单独分支）**：已在 `fix/control-write-detached` 修好并验收（含更严重的脉冲保持问题），见上文「运行时修复」。**未提交、未推送、没开 PR**。
- [ ] **运行时修复合并后**：把 `R3_EXPECT` 里 R3T1、R3T1X 改为 `(True, True)`，更新 CONTRACT §11.5 与 TEST-CASES，再跑 R3 组。
- [ ] **WP6 通用对账**：journal、`/audit`、假设备日志三方对账；"设备有写、journal 有意图、`/audit` 无条目"要归为"客户端断开后执行、IA2 漏记"，不是不明写入。
- [ ] **WP9 浸泡验收**：本机回环 2 小时混合请求浸泡，对账程序证明不明写入为 0，再逐项核对阶段 1a 退出条件。

## 剩余边界

- 阶段 1a 仍未退出：WP6、WP9 未做。
- `main` 上的 IA2 运行时漏记审计仍在（上表 R3T1、R3T1X），桥只是不再据此作错误断言；修复在另一分支、未合并。自然触发（不用 `SIGSTOP` 或注入停顿）未测。
- D11 事件／样本签名未实现；真实面向 Agent 的控制 MCP 未实现；真实硬件、断电持久性未验收。
