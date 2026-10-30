# 阶段 1a 收尾交接（PR #99 之后）

**本地后续工作：2026-10-30 UTC+8。** 用户确认保留 D11 签名；`feat/tier0-signed-control-mcp` 在 #101 提交基础上实现独立采样、强制验签和真实本地 stdio MCP。此增量尚未推送，不改变 #100/#101；交付与验证见 [LOCAL-CONTROL.md](LOCAL-CONTROL.md)、[VALIDATION-D11.md](VALIDATION-D11.md)。

**PR 提交状态：2026-10-30 UTC+8。** 运行时修复已提交为 [PR #100](https://github.com/FREEZONEX/ia2/pull/100)（`1cc16c0`）；本阶段 1a 工作已提交为 [PR #101](https://github.com/FREEZONEX/ia2/pull/101)，F1–F3 修复与封存文档提交为 `7a8f59a`，树与此前封存候选一致。两条分支均推送至 `FREEZONEX/ia2`；**先合 #100，再合 #101**。本次没有合并 PR，仍待审阅。提交前又重跑了 335 项 Python 测试、64 + 3 个变异及第二轮复核的 7 个 CLI 用例，全部通过；运行时分支全仓检查也通过（Rust 8594 通过 / 105 忽略 / 0 失败，Web 423 项）。新证据位于仓库外 `tier0-pr-submit-20261030/`。

**此前本地封存：2026-10-30 UTC+8。** 第二轮独立复核的 F1–F3 修复及收尾记录见 [`RESULT-1A.md`](RESULT-1A.md) §9、[`VALIDATION.md`](VALIDATION.md) 与 `validation-summary.json` 的 `rereview2_20261030`。对账工具为 `8274acdf…`，当前为 335 项 Python 测试、69 项对账向量、64 个对账变异；契约 v0.13。本地证据在 `evidence/rereview2-20261029/` 与 `evidence/rereview2-closeout-20261030/`，补丁、版本及哈希清单在后者。封存记录里的「未提交、未推送」是当时状态，原记录保持不变；当前 PR 状态见上段。

日期：2026-10-29 UTC+8。上一轮（IR03、`failsafe_status: null` 兼容、D6）的交接见本文件的 git 历史（`2d68f5b`）。

[PR #99](https://github.com/FREEZONEX/ia2/pull/99) 已于 2026-10-29 由维护者合并（`a2dec2a`）。合并前维护者补了一个提交 `f44755a`：harness 改用 `subprocess.Popen` 管理 runtime，并在线程里等它退出，避免 macOS 上 Python 3.14.7 的 asyncio 子进程监视器在 `SIGSTOP` 时卡住事件循环。本轮在合并后的 `main` 上，把 #99 留下的验收待办做完：R3 时序与对账语义、WP6 对账程序、P1 用例、WP9 两小时浸泡，并逐条核对阶段 1a 的退出条件。**结论与证据见 [`RESULT-1A.md`](RESULT-1A.md)：五条退出条件都有了证据。** 其中有几处是运行后改的 EXPECT 或判据，都已登记。

以下是 2026-10-29 的历史交付记录；当时两个分支均未推送，现已提交 PR，见本页顶部。

**同日晚的审核修复**：审核指出对账程序两条漏检路径（FIX01 替换、FIX02 设备日志覆盖）、运行时修复里的脉冲截短（FIX03）与浸泡统计口径（DOC01），都已修复并重新验证，结果见 [`RESULT-1A.md`](RESULT-1A.md) §7。对账结论多了 `INCOMPLETE`（退出码 2）；假设备日志每条带 `seq` 与 `mono`；新用例 WP6C；`lab/check_pulse_hold.py`；`tools/check_reconcile_mutations.py`。`R3_EXPECT` 已改为修复后的「执行且有条目」，所以本分支依赖运行时分支。修复者与作者是同一人，等复核。

## 交付定位

| 项目 | 内容 |
|---|---|
| 仓库 / 基线 | `FREEZONEX/ia2` / `main` `a2dec2a`（Rust 源码与 `e6a38aa` 相同） |
| 本分支 | `fix/tier0-r3-audit-reconcile`：第一个提交是 R3 时序与对账语义（下文「R3 时序覆盖」），第二个提交是 WP6、P1、WP9 与文档；只改 `examples/tier0-control/` |
| 运行时修复 | 分支 `fix/control-write-detached`（worktree `~/Projects/soft-PLC/ia2-control-write-detached-20261029`），本地提交 `48dec8a`，加审核修复 `1cc16c0`（脉冲从落地起计时）；见下文「运行时修复」 |
| 结果与退出条件 | [`RESULT-1A.md`](RESULT-1A.md) |
| 对账程序（WP6） | [`tools/reconcile_report.py`](tools/reconcile_report.py)；向量 [`tests/test_reconcile_report.py`](tests/test_reconcile_report.py)（69 项）；变异 [`tools/check_reconcile_mutations.py`](tools/check_reconcile_mutations.py)（64 个） |
| 浸泡（WP9） | [`lab/soak.py`](lab/soak.py)、[`lab/hooks_soak.py`](lab/hooks_soak.py) |
| P1 用例 | [`lab/cases_p1_table.py`](lab/cases_p1_table.py)；注册表检查加了快照名单（[`lab/check_registry.py`](lab/check_registry.py)） |
| 契约 | [CONTRACT.md](contract/CONTRACT.md) v0.13（§8.3 完整先后组合与共同交接时刻、§11.6 实测）；[TEST-CASES.md](contract/TEST-CASES.md) v0.9（§3.2）；[DESIGN-1A.md](contract/DESIGN-1A.md) v0.8 |
| 验证详情 / 机器可读结果 | [VALIDATION.md](VALIDATION.md)、[validation-summary.json](validation-summary.json) |

## 本轮结果

| 验证 | 结果 |
|---|---|
| Python 单元与进程测试 | 288 项通过（28 个套件），含对账程序 22 项向量；审核修复后 314 项（向量 48 项） |
| 对账程序的变异对照 | 16 个改坏的版本全部被抓到（`evidence/reconcile-mutations.log`）；审核修复后 39 个，全部被断言抓到（`evidence/reconcile-mutations/`） |
| 最终代码整套回环（28 个 P0、`PROD`、P1F5、P1F6、13 个 P1，R3 组三遍） | 54/54 PASS，727 条断言 0 失败（`evidence/final-20261029/`，代码树 `255e1166…`）；逐动作崩溃恢复 3/3、BOOL 21 项、nullable-failsafe 变异 3/3 |
| Python 3.14.8 上 R3 组与 R1、R2 | 7/7 PASS，没有卡住 |
| 2 小时浸泡（`main` 的 runtime） | 121 分钟、请求循环 4496 次（文件 4848 行，新动作 3999 个）、40 次故障；最终程序对账 CLEAN：2179 个 intent 中 2166 个一致，13 个差异各对应一次注入的故障，不明写入 0。浸泡当场的对账报了 1 处设备侧差异，分析后加了一条判据（RESULT-1A §3，**运行后改判据**） |
| 60 分钟浸泡（修好的 runtime `b7e455d4…`） | 1128 个 intent 中 1125 个一致、3 个没发出（R1）；两次 R3T1 精确点都有审计条目；PASS |

运行后改的地方（都已登记）：R3（上一提交，v0.8）；B5、C5、C10、R5、F4 的 EXPECT（TEST-CASES §3.2）；对账程序的「链路一直断着直到值被替换」判据（RESULT-1A §3）。

两次浸泡和最终整套是同时跑的，各用一组端口（`IA2_LAB_PORT_BASE`）。浸泡从仓库外的冻结副本运行（`~/Projects/soft-PLC/pr99-r3-timing-20261029/soak-tree*`，哈希清单在旁边）。保持唤醒的请求被拒，浸泡期间靠会话一直在跑来防止睡眠。

## 运行时修复（另一分支，本地提交，未推送）

`/audit` 漏记的根因在 IA2 运行时，按待办单独修，没有动本分支：

| 项目 | 内容 |
|---|---|
| 位置 | worktree `~/Projects/soft-PLC/ia2-control-write-detached-20261029`，分支 `fix/control-write-detached`，本地提交 `48dec8a`（父提交 `a2dec2a`）；差异与过闸门的补丁逐字节相同（`f8979d52…`），`a2dec2a` 没改任何 Rust 文件 |
| 读码新发现（更严重） | `monitor::write_with_pulse` 在调用方的 future 里等 `start_pulse`：调用方在脉冲写入排队后被丢弃时，写入照样落地，但复位任务从未建立，**值一直保持**——正是 `pulse_ms` 要防的事。IDE 服务与边缘 runtime 共用这段代码，两边都受影响 |
| 修复 | 脉冲写入与复位放进同一个独立任务；runtime 的 `/write`、`/force`、`/unforce` 把命令与审计放进独立任务（`run_detached`：handler panic 仍交给 CatchPanicLayer，运行时关停时答 503） |
| 回归 | `pulse_reset_survives_a_caller_dropped_while_the_write_is_queued`（bridge 库）、`control_writes_dropped_while_queued_are_still_audited`（runtime）；修复前分别得到"值停在 1"与"`/audit` 为空而 x=7"，修复后通过，各重复 15 次全过 |
| 文档 | `docs/api.md`（脉冲约定、`/audit` 行）、技能 02/03 |
| 仓库检查 | 适配、格式、Clippy、server 构建、发布构建、Web 构建、Web 423 项均过；Rust 全量 `--no-fail-fast` **8591 通过 / 0 失败 / 105 忽略**（检查脚本里第一次 `cargo test` 在 `sim_e2e` 的一个用例上报"server 5 秒内没起来"并就此停下，重跑全量未复现） |
| 端到端 | R3 组 ×3：R3T1、R3T1X 为 FINDING（执行且有审计条目），0 条断言失败；60 分钟浸泡见上 |
| PR 说明草稿 | 仓库外 `~/Projects/soft-PLC/pr99-r3-timing-20261029/PR-BODY-runtime-fix.md`；报告 `RESULT-RUNTIME-FIX.md` |

## 复跑

仓库根目录，Python 3.8+，无需安装依赖：

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py
python3 -I -B examples/tier0-control/tools/check_failsafe_controls.py
```

回环整套、P1、对账与浸泡的命令见 [README.md](README.md)。用例共用一组回环端口，须串行；要并行就给每套不同的 `IA2_LAB_PORT_BASE`。每次用新的证据目录。R3 时序用例在 macOS 上记录（`netstat -an -p tcp`），Linux 的 `netstat -tan` 形式未测。

## 待办

- [x] R3 时序覆盖与判据（上一提交）。
- [x] IA2 运行时审计修复（单独分支，本地提交）。
- [x] WP6 通用对账；P1；WP9 浸泡；退出条件逐条核对（`RESULT-1A.md`）。
- [x] **推送并开 PR**：用户于 2026-10-30 要求核对后提交，已在 `FREEZONEX/ia2` 提交 #100、#101。
- [ ] 审阅并按 #100 → #101 顺序合并；本分支的 R3 期望依赖运行时修复。
- [x] `R3_EXPECT` 里 R3T1、R3T1X 改为 `(True, True)`，CONTRACT §11.5 与 TEST-CASES 同步，R3 组在修好的 runtime 上跑过（审核修复时做了，见 `RESULT-1A.md` §7）。
- [x] 审核修复 FIX01、FIX02、FIX03、DOC01（`RESULT-1A.md` §7；仓库外的逐项回复 `REPAIR-RESPONSE-2026-10-29-tier0-stage-1a.md`）。
- [x] 第一、第二轮独立复核已完成并提出缺口；修复及本地收尾见 RESULT-1A §8、§9。
- [ ] 复核者检查 F1–F3 修复候选（本轮复跑既有脚本，不等同于新的独立复核）。
- [ ] 未决问题清单（`RESULT-1A.md` §6）里要你决定的：是否复议 D1；1b 的授权。

## R3 时序覆盖（本分支第一个提交）

### 时序结果（实测，3/3）

| 用例 | runtime 卡在哪里 | 执行了吗 | `/audit` | 桥最终对账（修复后） |
|---|---|---|---|---|
| R3 | 还没读到请求（`Recv-Q` > 0） | 否 | 无 | `absent`、`landed: null` |
| R3T1 | 已读完并排队，扫描循环还没执行（1 s 注入停顿中 `SIGSTOP`） | **是**（VM + 设备） | **无** | `absent`、`landed: null` |
| R3T1K | 同 R3T1，但 runtime 一侧连接保持打开（对照） | 是 | 有 | `found`、`landed: true` |
| R3T1X | 同 R3T1，改由桥 `SIGKILL` 触发 | **是** | **无** | `unknown / bridge_restarted`；`absent`、`landed: null` |
| R3T2 | 已执行并应答，响应没送到桥 | 是 | 有 | `found`、`landed: true` |

原因（读码，R3T1K 对照证实）：`crates/runtime/src/main.rs` 的 `rt_write` 在 `write_with_pulse(...).await` 之后才写审计；`send_write` 先把命令放进通道再等应答；hyper 1.9 默认 `h1_half_close = false`，忙时读到 EOF 就丢弃处理函数。`rt_force`、`rt_unforce` 同样写法（未运行）。这是 IA2 运行时缺陷，与 `rt_audit` 文档不符，修复在单独的分支（上文「运行时修复」），本分支不改 Rust。

### 修复内容

- **对账记录**：`reconcile_note` 与 journal 的 `reconcile` 记录新增 `audit_entry`（`found` / `absent` / `unavailable`）；`landed` 只在 `found` 时为 `true`，其余为 `null`，**不再写 `false`**。`detail` 写明"无条目不证明没有执行"及原因。两次对账的节奏、`final` 的含义、状态不升级、不重发均不变。
- **兼容**：旧桥写的 `landed: false` 第一次对账记录仍会触发第二次对账；读侧把旧 `landed: false` 读成 `absent`、`landed: null`；畸形字段不构成任何断言。
- **恢复**：崩溃恢复找不到条目时状态仍为 `unknown / bridge_restarted`（或 `runtime_restarted`），`detail` 注明"不证明没有执行"。
- **断言**：R1、B11 的"最终 `landed: false`"改为 `absent` / `null`；R3 的 EXPECT 按 3 遍实测改写（请求未读走时不执行），并加 `Recv-Q`、VM、设备日志核对；新增 4 个 P0 时序用例。按 TEST-CASES §2 在变更记录登记"运行后改 EXPECT"。历史 FINDING 留证不动。R3T1X 的桥侧断言跟随重启时 `/audit` 的实际内容（有条目则按 D8 收尾为 `accepted`、读侧 `verified`），所以换上修好的 runtime 时只报 FINDING，不误报 FAIL。
- **trace 启动读取**：原先已有 trace 里含非 ASCII 字节的行、或不是 JSON 对象的 JSON 行（如 `[1]`），会让桥以未捕获异常结束启动（不是 §8.4 的任何退出码），而非 JSON 的行却被静默跳过。现在凡是桥不可能写出的完整行（非 ASCII、不是对象、事件 ID 不是字符串、嵌套过深）一律跳过、不据其去重或取锚，并在 stderr 报出行号；锚只认非负整数 `journal_seq` 加字符串 `journal_hash`。CONTRACT §8.4 的 trace 段与 README 已同步。修复前新测试 4 项全部报错或失败（进程测试里桥打印回溯后退出），留证 `evidence/trace-reload-before/`。

## 剩余边界

- `main` 上的 IA2 运行时漏记审计仍在（R3T1、R3T1X），修复在另一分支、未合并；桥只是不再据此作错误断言。自然触发（不靠 `SIGSTOP` 或注入停顿）未测。
- 对账程序区分「客户端断开后执行」与「没有效果」要靠设备日志，这只在实验室有。
- 此前未实现的 D11 事件／样本签名和真实控制 MCP 已在后续本地分支实现，见 [VALIDATION-D11.md](VALIDATION-D11.md)；P2 的 C11、A3b 没做；真实硬件、现场总线时序、断电持久性未验收；没有碰 grey0 或真 Tier0。
- 其余未决问题见 `RESULT-1A.md` §6（D1 的代价、静默设备窗口、突发请求会过期、几种「答 200 ≠ 生效」、桥的内存随 journal 增长）。
