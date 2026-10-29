# 阶段 1a 结果（WP9）

日期：2026-10-29 UTC+8。被测 IA2：`main` `a2dec2a`（Rust 源码与 `e6a38aa` 相同，即 PR #98 的合并提交），runtime 二进制 SHA-256 `52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de`。全部在这台 Mac 的回环端口上，假 Modbus TCP 从站；没有连 grey0、真 Tier0 或任何硬件。

**结论：阶段 1a 的五条退出条件（[TEST-CASES.md](contract/TEST-CASES.md) §4）都有了证据，见下表。** 这只说明契约与桥在本机回环上站得住；第 6 节列出 1a 证明不了的东西和待决问题。

**同日的审核修复（§7）**：审核找出对账程序的两条漏检路径（FIX01、FIX02）与运行时修复里脉冲被截短的回归（FIX03），另有浸泡请求数的口径问题（DOC01）。都已修复并重新验证；两次浸泡与 A4 用修复后的对账程序重算后结论不变（仍为 `CLEAN`）。修复由同一个人完成，尚待复核者检查。

| # | 退出条件 | 结果 | 证据 |
|---|---|---|---|
| 1 | 全部 P0 为 PASS；FINDING 已回改契约并重跑 | **满足**：28 个 P0（含 R3 的四个时序用例）在最终代码上 PASS；R3 的 FINDING 已按 TEST-CASES §2 改 EXPECT 并重跑 | §1 |
| 2 | P1 为 PASS，或逐条写明 | **满足**：19 个 P1 里 14 个端到端 PASS，C9 由 `main` 本身覆盖，E4–E7 由验证器表驱动向量覆盖；与表不同之处逐条写明 | §2 |
| 3 | 2 小时浸泡后不明写入 = 0，journal 与 `/audit` 的差异全部能解释 | **满足**：不明写入 0；journal 与 `/audit` 的差异 13 处，全部对应到注入的故障（客户端断开后执行 6、未发送 5、请求未被读走 1、实例重启 1）。浸泡当场的对账另报了 1 处设备侧差异，分析后加了一条判据（见 §3），**这是运行后改判据** | §3 |
| 4 | C3、C4 数据交给用户；D1–D8 已定 | **满足**：数据见 §4（与 `RESULT-P0.md` 一致）；D1–D8 于 2026-10-27 定下 | §4 |
| 5 | 未决问题清单随结果交付 | **满足** | §6 |

## 1 P0

最终代码树（`code_tree_sha256` `255e1166af05f67fb3e3317409361fa974b8f1feff093740cf4d7158c76cbea2`）上整套回环用例 与 13 个 P1 一起跑：28 个 P0、`PROD`、P1F5、P1F6、13 个 P1，加上 R3 组再两遍，共 54 个 verdict **全部 PASS**，727 条断言 0 失败，无启动错误（`evidence/final-20261029/`）；逐动作崩溃恢复 3/3、BOOL 假设备 21 项、nullable-failsafe 变异对照 3/3；Python 单元与进程测试 287 项。R3 组（R3、R3T1、R3T1K、R3T1X、R3T2）另跑两遍，三遍合计 15/15 PASS；Python 3.14.8 上 R3 组与 R1、R2 各一遍 PASS（审核时在 3.14.7 上复现过的卡住问题已由 `f44755a` 修好，新加的时序用例也不受影响）。

**FINDING 回改清单**（运行后改 EXPECT 的地方，均登记在 TEST-CASES 变更记录）：

| 用例 | 原 EXPECT | 实测与处理 |
|---|---|---|
| R3 | 排队的请求在 `SIGCONT` 后落地 | 请求没读走时不执行；读走并排队的会执行但 `/audit` 无条目（R3T1、R3T1X）。EXPECT 改写（v0.8），对账记录不再写 `landed: false`（CONTRACT v0.9）。IA2 漏记审计是运行时缺陷，修复在单独的分支（§6） |
| B5 | (a) 404 → `unknown_variable` 并禁用；(b) 409 → `scan_stopped` | (a) 启动预检就拒绝这份注册表（退出码 4）；关掉预检后才走到 404 → `unknown_variable` 并禁用，之后的请求 `not_in_registry`。(b) `/stop` 之后 G8 先看到 runtime 不可达，`rejected runtime_unreachable`、不触达 IA2；409 的映射只在单元向量里 |
| C5 | 正常模式 `rejected paused` | runtime 暂停时扫描计数不动，G8 先拒（`runtime_unreachable`）；被强制时 G9 拒 `forced` |
| C10 | (e) runtime 重启后桥才拒 | 桥每个请求前都重读文件，改文件后的下一个请求就 `rejected northbound_write_enabled`；runtime 重启这一步不需要，没做 |
| R5 | 1 s 内 50 个请求触发全桥上限 | 单靠突发到不了上限：桥一次读一行（这里约每秒 7 个写入），读到时距 `requested_at_ms` 超过 2 s 的请求按 G4 `expired`（第一次运行：12 个 `expired`、0 个 `rate_limited`）。改为先在同一分钟里写 55 次再突发 |
| F4 | 检查工具指出名字不符 | 工具原来把实例前缀剥掉再比，`main_inst.sp_flow` 漏报（GAP）；已加可选的运行中快照名单，逐字比对 |

## 2 P1

| 用例 | 结果 | 说明 |
|---|---|---|
| A3 | PASS（表征） | 直连 runtime 时 `origin` 原样记录：`.` `-` `_`、`:`、空格、70 个字符都保留；空串和不带头都记 `anonymous`。桥只发 `[A-Za-z0-9._-]{1,64}`。经 IDE server 代理的 A3b（P2）没做 |
| A4 | PASS | 经桥写 300 次（全部 `accepted`），`/audit` 只剩最新 256 条、最早 44 条丢失。只凭最后一次读取，对账程序把这 44 个判为 `uncheckable_turnover`，既不说没写过、也不说一致；每秒归档的版本 300 个全部 `consistent` |
| B5 | PASS（EXPECT 已改，见 §1） | |
| B10 | PASS | 两个方向都 `rejected simulation_mismatch`，不触达 IA2 |
| C2 | PASS | G8 之后、发送之前看门狗跳闸：IA2 照样答 200，桥判 `unknown latched_after_write` |
| C5 | PASS（EXPECT 已改，见 §1） | 诊断半边由直连 runtime 表征：暂停时直写答 200、VM 收下、设备没收到；被强制时直写答 200 且回显请求值，VM 仍是强制值、设备没收到——又一处「答 200 ≠ 生效」，桥的 G8/G9 都在发送前拦下 |
| C7 | PASS | coupler 不健康而 `/health.status` 仍是 `ok`：`rejected device_unhealthy`，`/audit` 为空 |
| C9 | 由 `main` 覆盖 | `e6a38aa` 就是 #98 的合并提交，P0 的 C1、C3、C8 都跑在它上面。锁存时 `/health`、`/status` 都带非空 `failsafe_status`（两台设备 `acknowledged`），G8 拒 `latched`；正常时为 `null`（nullable-failsafe 兼容已处理） |
| C10 | PASS（(e) 见 §1） | `allow_write=true`、无法解析、无法读取：拒绝启动，退出码 3；文件不存在、`allow_write=false`：能启动 |
| R4 | PASS | hold 期间 `rejected held`，跨桥重启保持；之前已执行的写入不受影响；解除后恢复 |
| R5 | PASS（设置已改，见 §1） | 同目标 1 个放行、9 个 `busy`；其余 40 个里 4 个补满全桥 60 次/分，36 个 `rate_limited`；`/audit` 条数 = 放行数 |
| E4–E7 | 单元向量覆盖 | `tests/test_verifier.py`：容差边界（含 ±1 ulp 的包含规则）、窗口外 `no_proof_by_deadline`（E4）；采样器时钟偏移不能把旧样本变新或反之（E5 的读侧部分；请求时钟偏移由 P0 B6 端到端覆盖）；他人写的 `verified` 不算数（E6）；同一样本 `watchdog_tripped` 不算证据（E7）。没有端到端：验证器是纯函数，端到端用的样本来自 P0 E1/E2 已用过的同一个采样器 |
| F3 | PASS | 改一个字节：退出码 2，报出两个哈希 |
| F4 | PASS（工具已补，见 §1） | 越出治理范围、大小写不同、带实例前缀都被指出 |
| F5 | PASS | `enabled` 且 `requires_approval`：退出码 2 并指明该项；改成禁用后能启动，该项的请求在 IA2 之前被拒 |
| PROD | PASS | 生产包，见 P0 运行 |

P2 的 C11（MQTT 写的表征，要 broker）与 A3b（经 IDE server 代理，要假 ssh）没做。

## 3 浸泡（WP9）

`lab/soak.py`，`main` 的 runtime（`52160ab8…`），2026-10-29 13:39–15:40，种子 `20261029`，从冻结的目录副本运行（`evidence/soak-main-20261029/`，`soak-summary.json`、`reconcile.json`、`reconcile-final-program.json`）。

| 项 | 数 |
|---|---|
| 时长 | 121.1 分钟 |
| 请求 | 请求循环 4496 次：有效写入 2343、突发 352（每次写两行）、重复提交 487、治理拒绝 231、越界 247、未知键 144、伪造签名 203、畸形 159、过期 144、`simulation` 不符 99、类型错 87。请求文件 4848 行；其中签名有效的 4486 行（含重复提交 487），新动作 3999 个 |
| 结果 | 3999 个新的签名有效动作（不含重复提交）**全部有回答**；2179 个 intent（`accepted` 1934、`undelivered` 13、`unknown` 12、IA2 治理拒绝 220）；伪造与畸形的 362 行只进 journal（`unauthenticated`），没有一个触达 IA2 |
| 故障 | 40 次：设备 2 断链 / 静默 10、设备 1 静默 6、runtime 重启 4、`SIGSTOP` 5、看门狗锁存 2（各接一次重启）、R1 精确点 5、R2 精确点 1、R3T1 精确点 7；另有一次 12 分钟的归档暂停 |
| 进程 | 7 个 runtime 实例、7 次桥启动，全部是故障注入造成的；没有意外退出；journal 哈希链完好（10918 条）；桥从未写出 `verified` |
| `/audit` 归档 | 6500 次有效读取（11 次落在重启窗口里被丢弃），2166 条条目，**没有回绕缺口** |

对账（最终程序）：**CLEAN**。2179 个 intent 里 2166 个 `consistent`；13 个差异逐个对应到注入的故障：

| 类别 | 个数 | 对应的故障 |
|---|---|---|
| `executed_unaudited` | 6 | R3T1 精确点（7 次中的 6 次）：写入已排队、客户端断开后照样执行，`/audit` 无条目，设备收到了值——即 CONTRACT §11.5 的缺陷 |
| `no_effect_no_entry` | 1 | 第 7 次 R3T1：`SIGSTOP` 落在 runtime 读走请求之前（R3 的时序），没有执行 |
| `not_sent` | 5 | R1 精确点 5 次：intent 之后、发送之前杀桥；代理没看到 POST，重启后不补发 |
| `uncheckable_restart` | 1 | 写入后 0.5 s runtime 被杀，归档还没读到它的条目 |
| 不明写入 / 无法解释的设备写入 | 0 / 0 | |

**运行后改的判据（如实记录）。** 浸泡当场用冻结副本里的对账程序得出 `DISCREPANCY`，原因只有 1 处，而且在设备侧、不在 journal 与 `/audit` 之间：设备 2 被切成 `refuse`、连接已关闭 0.18 s 后，一个写 `d2_sp` 的请求到了；IA2 还没察觉断链，答 200、`undelivered_device: null`、审计 `ok`。7.6 s 后另一个写入替换了这个值（此时 IA2 已判 `undelivered`），链路 15.2 s 后恢复时 IA2 下发的是新值，旧值从未到达设备。设备自己的日志显示，从写入到被替换的整段时间里它一个请求都没有应答过（IA2 每 100 ms 轮询一次，链路正常时不可能空着）。所以加了一条判据：**写入之后到被替换（或日志结束）之间，该设备没有应答任何请求，即判 `link_down_until_replaced`**；有一个应答就仍是差异。它有单元向量（两个方向）和两个反向变异（都被抓到，变异合计 16/16）。两份报告都保留；用这条判据重对，两次浸泡里只有这一个动作用到它。这件事本身是 C4 那类 IA2 行为的又一例：链路已断而 IA2 尚未察觉时，写入答 200 却到不了设备（CONTRACT §11.6）。读侧对它给出 `unknown`（`runtime_restarted`），没有 `verified`。

**这条判据后来被审核指出过宽**（FIX02：没证明日志覆盖就从「没有应答」推断断链；FIX01：任何后续请求都被当作替换）。修复后的规则见 §7：替换只认确认应用的写入，断链要有显式的链路断开区间或连续的 `seq`。按新规则重算，这个动作仍是 `link_down_until_replaced`：设备 2 日志第 32861 行 `refuse`、第 32864 行 `normal` 的区间覆盖了从写入到替换写被答复的整段；替换写在同一实例的环里排在其后（第 987 条对第 986 条），`applied` 是另一个值，答复 200（补充记录 `evidence/repair-20261029/soak-main-link-down-supplement.json`）。

**同时进行的另一次浸泡**：修好审计缺口的 runtime（`b7e455d4…`，单独分支的修复），60.4 分钟，种子 `20261030`，端口基址 14500（`evidence/soak-detached-20261029/`）。请求循环 2252 次（文件 2440 行；签名有效 2289 行，新动作 2039 个）、17 次故障（含 R3T1 精确点 2 次），1128 个 intent：**1125 个 `consistent`、3 个 `not_sent`（R1），`executed_unaudited` 为 0**——两次 R3T1 的写入都记了审计；当场与最终程序都是 `CLEAN`，`soak-summary.json` 判 PASS。

**局限**：12 分钟的归档暂停里只来了约 216 条审计，没有写满 256 条的环，所以浸泡**没有**真正触发环回绕；回绕由 A4（经桥写 300 次，一次读取只剩 256 条，44 个判为不可核对）和单元向量确定性地覆盖。两次浸泡与最终整套用例是同时跑的（各用一组端口）；负载没有造成意外的看门狗跳闸或意外退出。浸泡从冻结副本运行：主浸泡的副本与最终代码只差后来改的对账程序及其测试、P1 用例、`run_cases.py`、`IA2_LAB_PORT_BASE`、`check_registry.py` 与文档；桥、`soak.py`、`hooks_soak.py` 都相同。

## 4 C3、C4 数据（复议 D1 的依据）

合并后这次整套运行的 C3、C4 与 `RESULT-P0.md`（2026-10-28，三遍）一致：

| 用例 | 数据 |
|---|---|
| C3 `close` | 链路恢复后 2.799 s 下发，晚于 deadline 3.099 s，距请求 7.099 s；下发的值就是请求值 `0x40a00000`；`/audit` 没有新条目 |
| C3 `refuse` | 恢复后 2.680 s、晚于 deadline 2.981 s、距请求 6.981 s；同上 |
| C4 | 静默开始到健康位翻转 8 次：1.162–1.216 s，中位 1.197 s。两轮各 15 个请求：13 个在窗口内被 `accepted`（IA2 当时还认为设备健康），17 个 `device_unhealthy`；被接受的 13 个读侧全部 `unknown feedback_stale`，没有一个 `verified` |

含义（不变）：D1(a)「不撤回」意味着一个被标为 `undelivered` 的写入，会在链路恢复后**秒级**迟到执行，而且 `/audit` 看不到这次下发；静默设备有约 1.2 s 的窗口，期间写入答 200 却到不了设备。

## 5 WP6 对账程序

[`tools/reconcile_report.py`](tools/reconcile_report.py) 只读文件，不导入桥的任何代码。分类按 CONTRACT §8.3 加上 R3T1 的教训：

- `consistent`：`/audit` 恰有一条、与 intent 和桥收到的答复一致，设备收到了应用的值（或有理由没收到：IA2 说未送达、设备已持有该值、设备收到请求却没应答、一个输出周期内被确认应用的新值或实例重启替换、设备日志证明从写入到被替换（或完整日志结束）之间没有可用链路；2026-10-29 审核修复后的规则见 §7）。
- `executed_unaudited`：归档覆盖了这次写入却没有条目，设备收到了值，桥没收到 HTTP 答复——客户端断开后执行、IA2 漏记（CONTRACT §11.5）。
- `not_sent` / `no_effect_no_entry`：没有条目、设备无效果；代理日志证明没发出，或发出后没有答复（R1 / R3）。
- `uncheckable_restart` / `uncheckable_turnover`：归档没能在该实例里、无缺口地看到它，且有实例重启或环回绕的证据。
- `uncheckable_archive` / `uncheckable_device_log`（审核修复新增）：归档停止看这个实例却没有它结束的证据；或判断需要设备日志的覆盖而日志没有覆盖。二者让结论成为 `INCOMPLETE`（退出码 2），不是 `CLEAN`。
- `unexplained`：其余一切（条目与 intent 不符、两条条目、有 HTTP 答复却无条目、设备收到却无条目而桥有答复、应用的值始终没到设备、同一标签两次 POST）。

另列：没有 intent 能解释的审计条目（**不明写入**），以及没有请求值、初值、程序自赋值或安全态能解释的设备写入。三者都为空、journal 哈希链完好、且没有上面两种证据缺口时才是 `CLEAN`。单元向量 48 项（审核修复前 22 项）；`tools/check_reconcile_mutations.py` 的 39 个人为改坏的版本都被断言抓到（`evidence/reconcile-mutations/`；修复前的 16 个见 `evidence/reconcile-mutations.log`）。

局限：归档条目没有 ID，相邻两条逐字相同的条目算一条（重发靠代理日志的 POST 计数查）；设备日志与生命周期文件是实验室才有的来源，生产环境里只有 journal 与 `/audit`，`executed_unaudited` 与 `no_effect_no_entry` 分不开。

## 6 证明不了的东西与未决问题

**1a 证明不了的**（TEST-CASES §4，不变）：物理效果与线上送达；EtherCAT、RTU 的真实时序（假从站是 Modbus TCP）；现役 HMI 试运行构建的行为；Tier0 的真实负载与重复摄入语义；断电持久性（Mac 上只测了进程被杀）。

**未决问题清单：**

1. **IA2 运行时漏记审计与脉冲保持**（R3T1 / R3T1X 的根因，及读码发现的更严重的脉冲复位丢失）。修复在单独的分支 `fix/control-write-detached`，未合并。合并前，`main` 的 runtime 上「客户端断开后执行的写入」没有 `/audit` 条目；只有本实验室的设备日志能把它们认出来（`executed_unaudited`），生产环境里只能记「不可核对」。审核修复时（§7）已把 `lab/cases_p0.py` 的 `R3_EXPECT` 里 R3T1、R3T1X 改成 `(True, True)`（CONTRACT v0.11 §11.5、TEST-CASES v0.10），所以本分支现在依赖那个修复；修复分支也修了审核发现的脉冲截短（FIX03）。自然触发（不靠注入停顿或 `SIGSTOP`，只是扫描循环忙）没测。
2. **D1「不撤回」的代价**（§4 的 C3 数据）：`undelivered` 的写入会在链路恢复后秒级迟到执行，`/audit` 看不到。请你据此决定是否复议 D1。
3. **静默设备窗口**（C4）：约 1.2 s 内 IA2 仍认为设备健康，写入答 200 却到不了设备；桥判 `accepted`，读侧永远不会 `verified`。这是 IA2 的健康判定节奏，不是桥能关掉的窗口。
4. **突发请求会过期而不是被限速**（R5）：桥一次处理一行，G4 只容许请求到达时与 `requested_at_ms` 相差 2 s；在这台 Mac 上每秒约处理 7 个写入，所以一次涌入的请求排到 2 s 之后的都会 `expired`。这是 fail closed，但 1b 前要确认这个吞吐与 Agent 侧的提交节奏是否相容。
5. **「答 200 ≠ 生效」的几种 IA2 行为**：锁存时（C1(b)、C2）、暂停时、目标被强制时（C5），IA2 都对 `/write` 答 200。桥在发送前用 G8/G9 挡住了这几种状态，但发送后才进入这些状态的写入只能靠发送后检查与读侧验证兜底。
6. **D11（trace 事件与样本签名）没有实现**；读侧验证器目前信任本地 trace 与样本文件。
7. **面向 Agent 的控制工具集（MCP / CLI）还不存在**，F1 只核对了桩工具集。
8. **对账程序依赖实验室来源**：设备写日志与生命周期文件只在实验室有。生产环境的对账只有 journal 与 `/audit` 归档，要靠运行时修复（第 1 条）才能让「无条目」重新有意义。
9. **桥的内存随 journal 线性增长**：浸泡中桥的 RSS 从约 23 MB 涨到 最高约 91 MB（2 小时、请求文件 4848 行，约每行 14 KB）。24 小时热索引期限只裁掉已完成动作的细节，`action_id` 与标签在 journal 的整个生命周期里都留在内存里（防重放），所以内存随 journal 增长；几天以上的长时间运行未测。
10. **阶段 1b 需要你另行授权**（触碰 grey0、真 Tier0），见 TEST-CASES §5。

## 7 审核修复（2026-10-29 晚）

审核意见（仓库外 `REPAIR-ADVICE-2026-10-29-tier0-stage-1a.md`）给了三项修复与一项文档修正；逐项回复与全部证据见仓库外 `REPAIR-RESPONSE-2026-10-29-tier0-stage-1a.md`，本机证据在 `evidence/repair-20261029/`。

| 编号 | 问题 | 修了什么 |
|---|---|---|
| FIX01 | 任何后续请求都被当作「已替换」，所以首写成功未送达、后写被拒时也给 `CLEAN` | 替换只认 `/audit` 证实已应用的后续写入：同一实例、环里排在其后、应用的是另一个值（取 `applied`）；同值再写不解除送达义务。实例重启要在一个输出周期内，或有断链证明直到重启 |
| FIX02 | 「没有应答」被当作断链，而没证明设备日志覆盖那一段；日志在 intent 前就结束也给 `CLEAN` | 设备日志要以 `listen` 开头、完整解析；`stopped` 后又有记录或 `seq` 断号是缺口；断链只认显式的链路断开 `mode` 区间或连续的 `seq`，且终点有限；覆盖不到就是 `uncheckable_device_log`；新结论 `INCOMPLETE`（退出码 2）。归档停止又没有重启证据时为 `uncheckable_archive`。假设备日志每条加 `seq` 与 `mono` |
| FIX03 | 运行时修复把脉冲截止时刻取在调用时，排队时间吃掉脉冲 | 运行时分支提交 `1cc16c0`：初写在调用方第一次 poll 时入队，截止时刻在写入落地后取 |
| DOC01 | 浸泡的「4496 行请求」其实是循环次数 | §3 与各文档按四种口径分开写 |

验证：

| 项 | 结果 |
|---|---|
| 新回归在被审核的程序上 | 48 项里 21 项断言失败、0 错误（`vectors-on-reviewed-program.log`） |
| Python 隔离套件 | 314 项通过（对账向量 48 项） |
| 对账变异 | 39/39 被断言抓到（`tools/check_reconcile_mutations.py`，含恢复旧 `next_same` 判据的那一个） |
| 两次浸泡与 A4 重算 | 仍为 `CLEAN`，计数不变；主浸泡 7 个 `lamp` 写入由 `already_held` 变为 `seen`（送达搜索窗口延到下一个确认应用的不同值）；唯一用到断链的动作按新规则仍成立，依据是设备 2 日志里显式的 `refuse` 区间（`soak-main-link-down-supplement.json`） |
| WP6C（新 runtime） | PASS（8 条断言）：日志编号连续、重启处恰有一段缺口、`CLEAN`；截在第三次写入前的副本为 `INCOMPLETE`、退出码 2 |
| 脉冲保持回环 | 新 runtime（`c18b0aaa…`）800 ms 脉冲保持 801.2 / 809.2 / 809.9 ms，100 ms 脉冲 110.1 / 109.3 / 109.0 ms，都在事先写定的带宽内；`main` 809.5 / 110.4，第一个修复提交 530.4 / 9.5 |
| 整套回环（新 runtime） | 55/55 PASS（28 个 P0、`PROD`、P1F5、P1F6、13 个 P1 与 WP6C，R3 组三遍），732 条断言 0 失败，代码树 `17a3e076…`（`evidence/repair-20261029/final-r2/`）；R3T1、R3T1X 各 3/3「执行且有条目」，与新 EXPECT 一致；逐动作崩溃恢复 3/3、BOOL 21 项、nullable-failsafe 变异 3/3；Python 3.14.8 上 R3 组与 R1、R2 7/7 |

由此，§6 第 1 条的运行时修复现在含 FIX03；`lab/cases_p0.py` 的 `R3_EXPECT` 已改为修复后的「执行且有条目」，所以本分支依赖运行时分支：用修复前 `main` 的 runtime 跑，R3T1、R3T1X 会报 FINDING。修好的 runtime 上没有新的 2 小时浸泡。修复者与作者是同一个人，等复核者检查。

