# 阶段 1 测试用例：Tier0 ↔ IA2 控制写入联调（草案 v0，2026-10-27）

配套契约：[`CONTRACT.md`](CONTRACT.md)（`ia2.control/v0.11`）。**历史 P0 验证记录：24 个 P0 用例已全部在 1a 夹具上运行（各 3 遍：23 个 PASS，R3 为 FINDING，无 FAIL；`tier0-ia2-control-1a-20261027/RESULT-P0.md`）；当时其余 19 个 P1 / P2 未运行；另有 `PROD`（生产打包的桥端到端，3 遍 PASS）。**

**阶段 1a 历史状态（2026-10-29，PR #99 合并之后）：** 28 个 P0、`PROD`、两个复核回归与 13 个 P1 在最终代码上 54/54 PASS（R3 组三遍），727 条断言无失败；WP6 对账程序与 2 小时浸泡已完成。**第 4 节的五条退出条件都有了证据**，逐条见 [`RESULT-1A.md`](../RESULT-1A.md)；其中 R3、B5、C5、C10、R5、F4 的 EXPECT 与浸泡对账的一条判据是运行后修订的，均有登记。

## 0 阶段划分

| 段 | 内容 | 触碰 | 状态 |
|---|---|---|---|
| **1a** | Mac 本机：沙箱 IA2 runtime + 假 Modbus TCP 从站 + 桥参考实现 + Tier0 替身（本地 JSONL）。本文第 3 节的全部用例 | 只用本机回环端口；不连 grey0、不连真 Tier0 | 退出条件都有了证据（`RESULT-1A.md`）；P2 的 C11、A3b 未做 |
| **1b** | 真 Tier0（grey0 上已在跑的六个容器）+ grey0 上**另起的**沙箱 runtime。第 5 节只列入口条件和追加用例 | 触碰 grey0 | **需要用户另行授权，本文不授权** |

1a 是 1b 的前置：真 Tier0 只会让问题更难定位，契约本身的错先在本机排掉。

## 1 被测物与夹具

**被测 IA2：** `FREEZONEX/ia2` 的 `origin/main` = `1c42df7`，子模块 `vendor/ironplc` = `80a1aa8`（v0.248.0）。C9 另测 PR #98 头 `5ca976e`。

**构建方式（来自本项目的教训，必须照做）：**
- 用 `git archive <显式 SHA>` 导出到 scratch 目录，子模块用 `git -C vendor/ironplc archive 80a1aa8` 单独导出；**独立** `CARGO_TARGET_DIR`；不要指向 `ia2/target`。
- **不要在 `~/Projects/soft-PLC/ia2` 里切分支或构建。** 写本文时该检出在 `fix/tap-supervision`，且有别的会话的未提交改动（包括 `docs/api.md`）；本文所有 IA2 事实都取自 `origin/main` 的对象，没碰工作树。
- 二进制名 `ia2-runtime`；记下 sha256 写进每个用例的留证。

**沙箱端口（回环）：** 沙箱 runtime `13501`；假从站 `13511–13513`；桥与 Tier0 替身不监听。`/stop`、`/pause`、`/force`、`/inject-scan-stall` **只**对自己起的沙箱实例发——脚本启动前核对目标端口属于自己起的子进程（沿用 `rehearse.py` 的"只信号自己起的子进程"做法）。若本机恰有到 grey0 的 SSH 转发，也不得让桥的配置指向它。

**复用（复制，不改原文件；这些目录有哈希清单）：**
- `bench-rehearsal-tcp-20261020/scripts/fakemodbus.py`（模式 `normal`/`silent`/`close`/`refuse`）、`mkproject.py`、`rehearse.py`。
- `tier0-acceptance-20261022/acceptance.py` 的 `derive_action` 与动作记录字段（读侧派生 `verified` 的逻辑；扩成 `CONTRACT` §4/§7 的新词汇）。

**待补的夹具缺口（阶段 1 实现前要补，见第 6 节）。**

## 2 判定与留证

| 判定 | 含义 |
|---|---|
| PASS | 满足"通过判据" |
| FAIL | 桥或契约本身错了（修桥/改契约后重跑） |
| FINDING | IA2 行为与 EXPECT 不符。**不是桥的失败**：回改 `CONTRACT` §6/§11，重跑受影响用例 |
| BLOCKED | 夹具缺口，用例没法跑 |

- **EXPECT 列在任何运行之前定下**，来自读码；运行后改 EXPECT 必须在末尾"变更记录"登记并说明。
- 优先级：**P0** ＝ 阶段 1 退出必须过；P1 ＝ 应过，不过要写明；P2 ＝ 表征性。
- 每例留证放 `runs/<id>-r<n>/`：请求 JSON、桥 journal、IA2 `/audit` + `/health` + `/status` 前后各一份、runtime 日志、假从站 jsonl、对账输出、二进制 sha256；目录生成后写 `MANIFEST.sha256`。
- 凡标"诊断模式"的步骤（关闭某个闸门的测试钩子）：只存在于测试构建的桥里，生产构建**不得**带；留证里记录该构建的哈希。

## 3 用例

### A 管道（无故障）

| ID | P | 场景（设置 → 动作） | EXPECT（读码） | 通过判据 |
|---|---|---|---|---|
| A1 | P0 | 注册 REAL 目标 `sp_flow`（0–50，治理规则同范围）；假从站把写入值镜像到一个 Input 通道（约 200 ms 延迟）；提交 12.5 | 200，`undelivered_device:null`，`value` = 请求；`/audit` 1 条 `ok`，标签含 action 前 8 位；反馈在 settle 内到达 | 桥 `accepted`；读侧重建 `verified`，`observation_refs` 指向反馈事件；journal 完整；`/audit` 恰 1 条 |
| A2 | P0 | REAL / DINT / BOOL 各取 {min, max, 中间值}；REAL 取 f32 舍入敏感值（贴近 max 的小数）；再试 LREAL 目标、min−ε、max+ε | IA2 按类型解释位编码：REAL 读回逐位相同；BOOL 0/1；LREAL 与越界在 G3 就被拒。BOOL 读回要线圈夹具（F-2）：现有假从站的 FC01 恒返回全零，WP0 里 `lamp_fb` 一直是 FALSE | 读回值等于请求（REAL 比位型）；LREAL/越界 `rejected` 且 IA2 无条目 |
| A3 | P1 | 直连沙箱 runtime，标签分别为：含 `.` `-` `_`、含 `:`、含空格、70 字符、空串 | 直连时 `origin` 为原样非空头值；空串 → `anonymous` | 记录实际行为；若与"合规字符集在直连和代理两条路径一致"的假设不符 → FINDING。（A3b，P2：经 IDE server 代理一条，需假 ssh 夹具，见第 6 节） |
| A4 | P1 | 向沙箱 runtime 写 300 次（不同标签）后读 `/audit` | 只剩最近 256 条，最早 44 条已丢 | 对账程序对丢失条目报"不可核对"，**不**报"没写过"，也不报成功；journal 300 条完整 |

### B 拒绝路径

| ID | P | 场景 | EXPECT | 通过判据 |
|---|---|---|---|---|
| B1 | P0 | 未登记键；直接给 IA2 变量名；`command` 取 force / unforce / stop / pause / resume / inject / alarm_ack / pulse；带 `pulse_ms` | G1/G2 拒（非 `ia2_set` 的命令与 `pulse_ms` 之类未知字段 → `bad_request`；未登记键 → `not_in_registry`）| 全部 `rejected`；窗口内 IA2 `/audit` **零新增**，runtime 日志无 `/write` `/force` `/stop` 等请求 |
| B2 | P0 | 值取 NaN、±inf、字符串、超大整数、REAL 越界、BOOL=2、-0.0 | G3 拒。**IA2 自己不拦这些**（WP0 实测）：BOOL=2 返回 200 且设备收到 ON；`+inf` 被 clamp 成 max；只有 NaN 是 403 | `rejected`（`bad_value`/`out_of_range`），IA2 无条目 |
| B3 | P0 | 把注册表范围放到治理白名单之外（或变量不在 allowlist）；提交 | IA2 403，审计有拒绝条目，VM 值不变 | `rejected governance_denied`；读回值不变；桥不重试 |
| B4 | P0 | 注册表 max=50，IA2 规则 max=40；提交 45 | IA2 clamp → 200，`value`=40 ≠ 请求；审计 `clamped` | `modified`，`applied_value=40`；永不 `verified`，哪怕反馈=40 |
| B5 | P1 | (a) 注册表指向不存在的变量名；(b) 在抛弃型沙箱实例上先 `POST /stop` 再提交 | (a) 404；(b) 409 "scan loop has stopped" | (a) `rejected unknown_variable`，该注册项被自动禁用；(b) `rejected scan_stopped` |
| B6 | P0 | deadline 已过；deadline 超过 `max_deadline_ms`；`requested_at_ms` 偏差 3 s | G4 | `rejected expired`；IA2 无条目 |
| B7 | P0 | 同一 action_id、同内容提交 3 次 | G5 | 三次返回同一结果；`/audit` 恰 1 条 |
| B8 | P0 | 同一 action_id、内容不同 | G5 | `rejected conflicting_action_id`；IA2 无新条目 |
| B9 | P0 | 提交 → 重启桥 → 同 action_id 重发 | journal 持久 | 不二次写，`/audit` 恰 1 条。（补 10-22 验收只有进程内去重的缺口） |
| B10 | P1 | 注册项 `simulation=true` 而请求 `simulation=false`，及相反 | G2 | `rejected simulation_mismatch` |
| B11 | P0 | 桩 IA2（可编程 HTTP 服务）依次回：500；非 JSON；200 缺 `undelivered_device`；200 缺 `value`；302；空体；半截 body 后断开；30 s 不回 | M9/M10 | 全部 `unknown`，**无一例 `accepted`**；缺键 ≠ null |

### C IA2 状态闸门与"确认 ≠ 生效"

| ID | P | 场景 | EXPECT | 通过判据 |
|---|---|---|---|---|
| C1 | P0 | `POST /inject-scan-stall` 令沙箱看门狗跳闸（`/health.watchdog_tripped=true`）。(a) 正常提交；(b) 诊断模式（测试钩子关闭 G8）提交 | (a) G8 拒；(b) IA2 回 200、`undelivered_device:null`（F2 预期），假从站**不会**收到该值（锁存时输出阶段被跳过） | (a) `rejected latched`，IA2 无条目；(b) 只表征——一致则记"F2 在 `1c42df7` 复现"（2026-10-28 已复现，3/3：200、`undelivered_device:null`、设备无写入、VM 变量变成请求值），不一致 → FINDING，改 §6 M4 |
| C2 | P1 | 钩子：G8 通过后、发送前触发看门狗 | 200 | `unknown latched_after_write`；不得 `accepted` |
| C3 | P0 | 假从站 `close` 或 `refuse`，等 `device_health` 翻 false 后提交；过 deadline；再让从站恢复。D1 已定 (a)（不撤回），只跑这一种 | 200，`undelivered_device` = 设备名；审计文本含 "not delivered … (link down)"；恢复后从站收到 VM 的**当前**值（WP0 实测：恢复后约 1.8 s 到达，值正是请求值，且 `/audit` 无新条目；单次观测） | 状态 `undelivered`。记录：恢复时刻、从站收到的值与时刻、距离 deadline 的迟到时长。预期收到迟到的命令值。**这组数据留作复议 D1 的依据**（迟到多久、值是否仍有危险） |
| C4 | P0 | 假从站 `silent`：静默开始后 0–1.5 s 内每 100 ms 提交一次，n ≥ 20 | 健康位约在静默起 1.1–1.2 s 后才翻（10-20 彩排）；翻之前 200 + `undelivered_device:null` | 窗口内的写停在 `accepted`，无反馈 → deadline 后 `unknown no_proof_by_deadline`，**无一例 `verified`**；把"静默开始 → 健康位翻转"的时延分布（中位/最大）写进结果 |
| C5 | P1 | (a) `POST /pause`；(b) `POST /force` 目标变量；各在正常和诊断模式提交 | 正常：G9 拒；诊断：200，(a) 值进 VM 但不扫描，(b) force 每周期胜出、反馈不变 | 正常 `rejected paused` / `forced`；诊断只表征 |
| C6 | P0 | 目标变量由程序每周期赋值（`out := …`）；提交 | 200；下一周期被覆盖；反馈不变（WP0 实测：设备从未收到该值——输出只在值变化时下发，所以设备日志里没有这次写入不等于没写） | 停在 `accepted` → deadline 后 `unknown no_proof_by_deadline`；注册表静态检查**应当**标出该变量——检查工具若还没有这项，记 GAP |
| C7 | P1 | 注册项 `require_devices=["coupler"]`，令 coupler `healthy=false` 而 `/health.status` 仍为 ok | G9 | `rejected device_unhealthy`；IA2 无条目 |
| C8 | P0 | `accepted` 后 `kill` 并重启沙箱 runtime（新 `runtime_id` / PID） | `/audit` 清空；新实例变量回到工程初值 | `unknown runtime_restarted`；不重发；新实例上匹配目标的反馈**不**使旧动作 `verified` |
| C9 | P1 | 对 #98 头 `5ca976e` 构建重复 C1、C3、C8 | 锁存时 `/health` 与 `/status` 含 `failsafe_status`；其余同 `1c42df7` | `failsafe_status` 非空时 G8 拒；与 `1c42df7` 的差异列表写进结果。字段名先读该 PR 头确认再动手 |
| C10 | P1 | 沙箱工程的 `northbound.toml`：(a) `[mqtt]` 里 `allow_write=true`；(b) 文件无法解析 / 无法读取；(c) 文件不存在；(d) `allow_write=false`；(e) 桥运行中把文件改成 `allow_write=true` 并令 runtime 重启（新 `runtime_id`） | D6：`/health`、`/status` 不暴露北向状态，只能读文件。runtime 把「文件不存在」「无法读取」都当作北向关闭，桥对「无法读取 / 解析不了」更保守 | (a)(b) 桥拒绝启动；(c)(d) 可启动；(e) 桥发现 `runtime_id` 变化后重读文件，之后的请求 `rejected northbound_write_enabled`，且不触达 IA2 |
| C11 | P2 | 沙箱 runtime 的 `allow_write=true` 且有本机 MQTT broker 时，向 `<prefix>/write` 发一条（只作表征；桥不参与） | MQTT 写被应用，审计 `origin=mqtt`，**无任何响应通道**，且绕过桥的全部闸门 | 记录。需要 broker（Mac 上没有 mosquitto，见 `DESIGN-1A.md` §5），可推迟到 P0/P1 之后 |

### R 桥自身故障

| ID | P | 场景 | EXPECT | 通过判据 |
|---|---|---|---|---|
| R1 | P0 | 钩子：意图写入 journal 后、发 HTTP 前 `kill -9` 桥；重启 | journal 有意图无结果 | 按标签查 `/audit`：无条目 → 保持 `unknown`、**不补发**（D8 已定 (a)）；IA2 `/audit` 0 条，重启后重发同 action_id 也不二次写（B9） |
| R2 | P0 | 钩子：已收到 200、写结果入 journal 前 `kill -9` | 审计有条目 | 重启后对账找到标签条目 → 补记对应状态，**不重发**；IA2 恰 1 条 |
| R3 | P0 | `SIGSTOP` 沙箱 runtime（尚未读走请求）超过桥的 HTTP 超时，再 `SIGCONT` | （v0.8 按实测修订；原 EXPECT「排队的请求在 CONT 后落地」不成立）请求不执行：VM 不变、设备无写入、`/audit` 无条目 | 挂起期间桥 `unknown runtime_unreachable`，不重发；`Recv-Q > 0` 证明时序；最终对账 `audit_entry: absent`、`landed: null`——不声称没有执行 |
| R3T1 | P0 | runtime 已读完请求、写入排在扫描循环里：注入一次 1 s 扫描停顿，放行桥，150 ms 后在停顿中 `SIGSTOP`，桥超时后 `SIGCONT` | 写入**照样执行**（VM 与设备），`/audit` **有条目**（v0.10 起按修复后的 runtime；`main` 修复前无条目，是 IA2 漏记，CONTRACT §11.5） | 同 R3 的桥侧要求；`Recv-Q = 0`；连接 `CLOSE_WAIT`；看门狗未跳；最终对账跟随 `/audit`：有条目为 `found` / `true`，无条目为 `absent` / `null` |
| R3T1K | P0 | R3T1 的对照：桥放弃后，代理让 runtime 一侧的连接保持打开 | 写入执行，`/audit` 有条目 | 对账 `found`、`landed: true`（第一次看时还没执行，可在第二次） |
| R3T1X | P0 | R3T1 的时序，改为在停顿中 `SIGKILL` 桥，再普通重启 | 写入执行，`/audit` 有条目（v0.10 起按修复后的 runtime；`main` 修复前无条目） | 判据跟随重启时 `/audit` 的实际内容：无条目时恢复为 `unknown bridge_restarted`，`detail` 不把无条目读成没执行，对账 `absent` / `null`；有条目时（会审计这种写入的 runtime）按 D8 从条目收尾为 `accepted`，不再有对账记录，读侧为 `verified`。两种情况都只有一次 POST |
| R3T2 | P0 | runtime 已执行并应答，代理扣住响应并同时 `SIGSTOP` runtime，桥超时后 `SIGCONT`、丢弃响应 | 写入执行，`/audit` 有条目 | `unknown runtime_unreachable`；对账 `found`、`landed: true`；不重发 |
| R4 | P1 | 放置 `hold`；提交；重启桥；解除 | G7 | `rejected held`；hold 跨重启保持；已发出的动作不受影响、不被撤销 |
| R5 | P1 | 1 s 内涌入 50 个请求（其中同目标 10 个） | G6 | 同目标只放行 1 个，其余 `busy`；全桥上限生效 `rate_limited`；IA2 `/audit` 条数 = 桥放行数 |

R3 组的共同判据：状态不升级；读侧仍为 `unknown` 且不出现 `landed: false`（R3T1X 在重启时找到条目的情况除外，见上表）；声称 `landed: true` 的记录必须有设备或 VM 的事实支持。IA2 的执行与审计结果按上表 EXPECT 比对，不一致为 FINDING：v0.10 起 EXPECT 记的是带运行时修复（分支 `fix/control-write-detached`）的 runtime，所以在修复合并前用 `main` 构建的 runtime 跑，R3T1、R3T1X 会报 FINDING（执行而无条目），这正是那个缺陷。

### E 读侧验证

| ID | P | 场景 | 通过判据 |
|---|---|---|---|
| E1 | P0 | 断链使 `input.stale=true`，而 VM 里的旧反馈值恰好等于目标 | 不 `verified` |
| E2 | P0 | 反馈观测早于 `requested_at_ms`（旧值碰巧等于目标） | 不 `verified` |
| E3 | P0 | 注册项没有 `verify.feedback` | 最高 `accepted`；deadline 后 `unknown no_feedback` |
| E4 | P1 | 反馈在 settle 内 / 外收敛；恰在容差边界 ± 1 ulp | 内 → `verified`；外 → `unknown no_proof_by_deadline`；边界按 `≤ tolerance` |
| E5 | P1 | 桥时钟与请求 `requested_at_ms` 偏移 ±1 s、±3 s | ±3 s `expired`；±1 s 不产生假 `verified` |
| E6 | P1 | 另一进程往 `Action/trace` 写一条 `status:"verified"`（无证据） | 读侧重建忽略它；动作状态不变 |
| E7 | P1 | 反馈匹配，但同一样本 `watchdog_tripped=true` | 不 `verified` |

### F 边界与回归

| ID | P | 场景 | 通过判据 |
|---|---|---|---|
| F1 | P0 | 列出控制入口暴露的工具与参数；对比既有只读 MCP 的 `delivery-lock.json` | 控制入口工具集 = 契约约定；只读 MCP 仍是 7 个工具、无写参数、文件哈希未变 |
| F2 | P0 | 伪造请求：无签名；错误签名；篡改 `target` 后沿用原签名；未知 `key_id`；过期后重放；期内重放；**签名正确但 payload 非法**（不是合法 JSON / 含重复键 / 缺字段 / 多余字段）；**占坑**：先发一条带某 `action_id` 的伪造请求，再发同 `action_id` 的真实签名请求 | 前四类 `rejected unauthenticated`，且**不进入去重表**；签名正确但 payload 非法 → `rejected bad_request`（先验签后解析）；过期 `expired`；期内重放走 B7 的去重；占坑后的真实请求正常执行；IA2 无多余条目；全部留证（journal、桥日志、trace、harness 日志）里扫不到密钥字节 |
| F3 | P1 | 改动注册表的一个字节 | 桥拒绝启动 |
| F4 | P1 | 注册表范围超出部署工程的治理规则；`ia2_variable` 与运行中 `/status` 快照里的变量名不逐字相同（大小写、实例名）（离线检查工具） | 检查失败并指出变量。（B4 是范围检查缺失时的运行时后果；名字不符的运行时后果是 B5(a) 的 404） |
| F5 | P1 | 注册表里有一项 `enabled=true` 且 `requires_approval=true`；启动桥 | D3：v0 没有批准层，预留字段必须 fail closed | 桥拒绝启动并指明该项；改 `enabled=false` 后可启动；该项的请求在任何情况下都不触达 IA2 |
| PROD | P1 | 用 `tools/package_prod.py` 生成的**生产包**起桥（不含任何加载钩子的代码），提交一个正常请求 | 与 A1 同样的 `accepted` → `verified`；journal 的 `start` 记录 `build = prod`、无钩子；包里搜不到 `test-hooks` / `testhooks` / `importlib`；线上恰一次 `POST /write` | 全部是 W6 的要求。其余所有用例跑的是 `lab` 构建 |

合计 48 个用例（v0.1 增加 F5、C11；v0.7 增加非 P0 的 `PROD`；v0.8 增加 R3 的四个时序用例），P0 共 28 个：A1 A2 B1 B2 B3 B4 B6 B7 B8 B9 B11 C1 C3 C4 C6 C8 R1 R2 R3 R3T1 R3T1K R3T1X R3T2 E1 E2 E3 F1 F2。

### 3.1 P0 运行中据实修订的地方（2026-10-28）

24 个 P0 用例跑完后，下列用例的实现与上面的表不完全相同。每一条都是有意的；EXPECT 的理由在各用例的检查名里。

| 用例 | 与表不同之处 | 为什么 |
|---|---|---|
| A2 | LREAL 目标不是被 `rejected`，而是**注册表加载即被拒、桥不启动**（退出码 2，线上无请求） | 注册表是人审的静态文件，类型不支持比请求时再拒更早、更安全；`unsupported_type` 在 v0 到不了请求路径 |
| B2 | `NaN` / `Infinity` 字面量不是 `bad_value`：它们不是严格 JSON，在 G1 就被拒，**只记 journal**（没有可信的 `action_id`）；`1e999` 才走到 G3 成为 `bad_value`。字符串、`null` 是 `bad_request` | 严格 JSON 加载器拒绝非有限常量；`json` 标准库默认放过它们 |
| C4 | 通过判据「无一例 verified」的前提是**每次写的值互不相同**。第一次运行里两轮用了同样的值，设备已有该值，于是有一例 `verified`——按规则这是对的，但它说明 `verified` 不证明「这次写入是原因」。已改 `not_claimed` 并加单元向量 | 见 `CONTRACT.md` §7 第 7 条 |
| C8 | 「设备保留旧值、新实例读回同值」这个陷阱**不会自然出现**：新实例启动时把设备输出写成 0。用例先记录这一点，再**人为把设备寄存器放回 12.5** 来检验读侧规则 | 观测；陷阱是人为设置的 |
| E1、E2 | 用「暂停采样器」控制哪些样本存在（E1：窗口内的新鲜样本被扣下，只剩过期样本；E2：从请求起到窗口结束都不采样）。这是验证读侧规则的端到端演示，不是自然发生的场景 | 否则新鲜样本会合法地让它们 `verified` |
| F1 | 只做了一半：检查的是**桩工具集**（`lab/toolset.py`）的工具面，加上既有只读 MCP 的 7 个工具与交付锁哈希；该历史轮次尚无真正的 Agent 面 MCP；本轮由 MCP / MCPPROD 用例覆盖真实服务 | 见 `DESIGN-1A.md` §8 第 7 条 |
| F2 | 伪造请求被记入 **journal**（`unauthenticated`），没有逐动作的 trace 事件；「无签名」是信封畸形，原因 `bad_request`。「过期后重放」分两种：从未执行过的签名请求 → `rejected expired`；已执行过的 → 复述记录的结果、不写 | 伪造请求声称的 `action_id` 不可信 |
| R2(A) | 从 `/audit` 收尾的动作现在**可以**被验证：重启时桥读 `/status`，用此刻的扫描时钟做标记，读侧只接受标记之后的反馈 | 写入发生在这次读取之前、同一个实例，所以标记是可靠的下界（`RESULT-FIXES.md` §3） |
| B2 | 加了一行 70 KB 的请求：超过 64 KiB 的行不读入，只记长度，记一次 `bad_request` | W8 |
| B3、B4 | 桥用 `--governance-check warn` 启动 | 这两个用例故意让注册表超出（B4）或绕开（B3）治理；默认的 `strict` 会拒绝启动（W11） |
| B11、R1、R3 | 对账看**两次**：B11 里每个 unknown 得到两条 note（第二条 `final`）；R1、R3 等 `final` note | W9 |
| R3 | EXPECT（排队的请求在 CONT 后落地）**不成立**：请求没有落地（`/audit` 为空），桥的对账记 `landed: false`（历史，2026-10-28）。2026-10-29 的时序覆盖证明 `/audit` 无条目不能说明没有执行（R3T1、R3T1X），EXPECT 与对账记录已在 v0.8 改写 | 见 `RESULT-P0.md`、CONTRACT §11.5 |

### 3.2 P1 运行中据实修订的地方（2026-10-29）

P1 由 `lab/cases_p1_table.py` 端到端运行（结果见 [`RESULT-1A.md`](../RESULT-1A.md) §2）。与上面的表不同之处：

| 用例 | 与表不同之处 | 为什么 |
|---|---|---|
| A4 | 写入经桥发出（journal 才有 300 条），并另开每秒一次的 `/audit` 归档作对照 | 判据要求 journal 完整、对账程序对丢失条目报"不可核对"；只有经桥的写入才有 journal |
| B5 | (a) 启动预检就拒绝指向不存在变量的注册表（退出码 4）；用例另起关掉预检的桥，才观察到 404 → `unknown_variable` 并禁用，之后的请求 `not_in_registry`。(b) `/stop` 之后 runtime 进程退出，G8 以 `runtime_unreachable` 先拒，不触达 IA2；409 → `scan_stopped` 的映射只在单元向量里 | 桥的检查比 EXPECT 设想的更早 |
| C5 | 正常模式下暂停：G8 先拒（扫描计数不动，`runtime_unreachable`），不是 G9 的 `paused`；强制：`forced`。桥没有跳过 G9 的开关，"诊断"半边改为直连 runtime 表征 | 闸门顺序 |
| C10 | (e) 不重启 runtime：桥每个请求前都重读文件，改文件后的下一个请求就被拒 | 比 EXPECT 更严；runtime 只在下次启动时采用新文件 |
| R5 | 先在同一分钟里分批写 55 次，再在 1 s 内涌入 50 个 | 单靠突发到不了全桥上限：桥一次读一行，读到时距 `requested_at_ms` 超过 2 s 的请求按 G4 `expired`（第一次运行 12 个 `expired`、0 个 `rate_limited`） |
| F4 | 检查工具新增可选的运行中快照名单，逐字比对；原工具剥掉实例前缀再比，`main_inst.sp_flow` 漏报（GAP，已补） | 判据要求指出名字不符 |
| C9 | 不单独运行：`main` `e6a38aa` 就是 #98 的合并提交，P0 的 C1、C3、C8 都跑在它上面 | 被测构建相同 |
| E4–E7 | 由验证器的表驱动向量覆盖，没有端到端 | 验证器是纯函数；端到端的样本来自 E1/E2 已用过的同一采样器 |

### 3.3 审核修复新增（2026-10-29）

审核意见 `REPAIR-ADVICE-2026-10-29-tier0-stage-1a.md` 之后新增；结果见 [`RESULT-1A.md`](../RESULT-1A.md) §7。

| 用例 | 级别 | 做法 | 判据 |
|---|---|---|---|
| WP6C | P1 | 经桥写三次，中间把假设备停掉再启动；之后把同一份设备日志截在第三次写入之前另存一份，两份分别对账 | 设备日志以 `listen` 开头、每条有连续的 `seq` 与 `mono`；报告列出且只列出从设备 `stopped` 到下一次 `listen` 的那一段缺口；完整日志对账 `CLEAN`、三次写入都 `consistent`；截短的日志让截点之后的写入成为 `uncheckable_device_log`、结论 `INCOMPLETE`、退出码 2，之前的两次仍 `consistent` |
| 脉冲保持（`lab/check_pulse_hold.py`） | — | 注入一次 300 ms 扫描停顿，60 ms 后写 `lamp`（`pulse_ms` 800 与 100），从假设备日志的 `mono` 量线圈置 1 到置 0 的时间 | 被测 runtime 每次都在 `[pulse_ms − 10 ms, pulse_ms + 50 ms]` 之内（任务周期 10 ms：置 1 最多晚一个扫描到设备，复位允许晚五个扫描）；带宽在运行前写定。参照构建只记录、不判 |

## 4 阶段 1a 的退出条件

1. 全部 P0 为 PASS；若有 FINDING，已回改 `CONTRACT` 并重跑受影响用例直到 PASS。
2. P1 为 PASS，或逐条写明未过原因与影响。
3. 一次 2 小时的随机请求浸泡（有效/无效/重复/故障注入混合）后，对账程序报告**不明写入 = 0**，且 journal 与 `/audit` 的差异全部能解释（环回绕、实例重启）。
4. C3、C4 的数据已交给用户（C3 的迟到数据用于复议 D1）；D1–D8 已于 2026-10-27 全部定下（`CONTRACT.md` §10）。
5. 未决问题清单随结果一起交付。

**即使全过，1a 也不能证明：** 物理效果和线上送达；EtherCAT 或 RTU 的真实时序（假从站是 Modbus TCP）；现役 HMI 试运行构建的行为（比 `main` 落后约 30 个 PR，输出 `input.stale` 与否都未核对——阶段 2 要在目标构建上重做 A1、C1、C3、C8）；Tier0 的真实负载与重复摄入语义。

## 5 阶段 1b（入口条件与追加用例；需用户另行授权）

**入口条件：** 1a 退出；用户明确授权触碰 grey0；沙箱 runtime 起在 grey0 的空闲回环端口、`nice 19`，**不碰**现役 `ia2.service`（PID 1831008 一族）；桥作为用户服务、不开机自启；Tier0 使用独立命名空间（`ia2_ctl_lab_<时间戳>`），不写既有命名空间；只读侧的采集器与 MCP 不改。

| ID | P | 追加用例 |
|---|---|---|
| T1 | P0 | 真 Tier0 的 Action 往返：Agent 控制入口 → `Action/requests` → 桥 → 沙箱 → `Action/trace` → `query_action_results`，A1/B3/B4/C3 各一遍 |
| T2 | P0 | Tier0 的 HTTP→MQTT 重复摄入（RESULT 记载同一事件会出现两行）不能引起二次执行 |
| T3 | P1 | 动作进行中重启 EMQX / 后端：不丢请求、不重复、不二次写，恢复后状态正确 |
| T4 | P1 | 桥 + Tier0 在线时对沙箱扫描节拍的影响（健康总线；只作粗测，与 #65 无关） |
| T5 | P0 | 2 小时浸泡：随机有效/无效请求 + 周期性故障注入，对账不明写入 = 0 |

## 6 夹具缺口（动手前要补的东西）

1. `fakemodbus.py` 的「写后回读」：**已核对（读码 + 它自带的自测）**。FC06/FC16 写入存进 `regs`，FC03/04 读同一个 `regs`，所以同地址的写通道 `ao1` 与读通道 `ao1_echo` 天然构成回读，够 A1、C3、E 组的大部分。**缺**：线圈（FC05/FC15 不存储、FC01/02 恒返回全零，BOOL 目标没法回读）；写日志对 FC16 只记第一个寄存器；写后延迟 / 冻结回读没有控制手段。
2. 新的工程生成器（新文件，不改 `mkproject.py`）：`[governance]` allowlist + min/max；REAL / DINT / BOOL 目标；被程序赋值的输出；带 `input.stale` 的反馈 Input 映射；多个 Modbus 设备。
3. 桩 IA2 HTTP 服务（B11）。
4. 桥参考实现及其 `--test-hooks`（G8 与发送之间停顿、意图后崩溃、关闭某闸门）。
5. 最小 MQTT broker 或本机 mosquitto（C10）。
6. 对账程序（journal ↔ `/audit` ↔ 假从站日志）。
7. 假 ssh 包装（A3b；#75 部署健康检查用过同类做法）。
8. 注册表静态检查工具（C6、F4）。

各缺口的现状证据、补法、规模和依赖见 [`DESIGN-1A.md`](DESIGN-1A.md) §5–§6。

## 7 变更记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0 草案 | 2026-10-27 | 首版；全部用例未运行；EXPECT 来自对 `1c42df7` 的读码。 |
| v0.1 | 2026-10-27 | 用户定 D1–D8（`CONTRACT.md` §10）。C3、R1 只剩一个分支（D1(a)、D8(a)）；C10 改为 D6 的自检（P1），原 MQTT 表征拆为 C11（P2）；F2 增加「占坑」与密钥扫描；新增 F5（D3 fail closed）；退出条件 4 改写；夹具缺口 1 已核对。EXPECT 其余未变，仍无任何运行。 |
| v0.2 | 2026-10-27 | 用户定 D9（JSON 注册表）、D10（对 payload 精确字节签名）：F2 增加「签名正确但 payload 非法」变体。其余未变，仍无任何运行。 |
| v0.3 | 2026-10-27 | WP0 实测后据实修订 EXPECT（**不是用例运行**）：B2、A2、C3、C6 加上观测到的 IA2 行为；F4 增加变量名逐字核对。判定标准未变。 |
| v0.4 | 2026-10-28 | A1、B1、B3、C1 运行后：B1 EXPECT 写明 `bad_request` / `not_in_registry` 的划分；C1(b) 记录已复现。判定标准未变。 |
| v0.5 | 2026-10-28 | 24 个 P0 运行后：新增 §3.1（运行中据实修订的地方）；B7、B9 增加「journal 里这个动作只有四条记录」的检查（变异检查发现去重会被单飞门掩盖）；C4 要求每次写的值互不相同；R3 结果为 FINDING。判定标准未变。 |
| v0.6 | 2026-10-28 | 复核前的畸形输入探针：新增 `tests/test_robustness.py`（25 种畸形签名输入、`internal_error` 隔离、重启重读不重复记）作为 F2 / B2 之外的单元级补充；24 个 P0 在修后的代码树上重跑 3 遍，结果不变（23 PASS、R3 FINDING）。 |
| v0.7 | 2026-10-28 | 修复 W1–W19 后：新增非 P0 用例 `PROD`；B2 加超长行；B3、B4 用 `warn`；B11、R1、R3 配合对账两次；R2(A) 变为可验证；harness 不再把桥启动期间的读取算作请求流量。24 个 P0 + `PROD` 各 3 遍的结果见 `RESULT-P0.md`。判定标准未变。 |
| v0.10 | 2026-10-29 | 审核修复：R3T1、R3T1X 的 EXPECT 改为修复后 runtime 的「执行且有条目」（`main` 修复前的 runtime 上为 FINDING）；新增 §3.3（WP6C、脉冲保持回环）；浸泡的请求数按口径分开写（`RESULT-1A.md` §3）。对账判据见 `CONTRACT.md` v0.11 §8.3。 |
| v0.9 | 2026-10-29 | P1 端到端（`lab/cases_p1_table.py`：A3 A4 B5 B10 C2 C5 C7 C10 R4 R5 F3 F4 F5），**运行后改 EXPECT** 的地方见 §3.2（B5、C5、C10、R5、F4）；A4 的判据由 WP6 对账程序检验；新增 WP9 浸泡（`lab/soak.py`），退出条件 3 的证据见 `RESULT-1A.md` §3。浸泡后**对账程序加了一条判据**（设备日志显示链路一直断着直到值被替换，则旧值没到设备不算差异），理由与两份报告见 `RESULT-1A.md` §3。判定标准未变。 |
| v0.8 | 2026-10-29 | R3 时序覆盖（HANDOFF 待办）：**运行后改 EXPECT**——R3 原 EXPECT「排队的请求在 CONT 后落地」与 3 遍实测不符，改为实测的「请求未读走时不执行」，并加 `Recv-Q`、VM、设备日志的核对；新增 R3T1、R3T1K、R3T1X、R3T2（P0）。实测发现 runtime 读完并排队的写在客户端断开后照样执行却不进 `/audit`（CONTRACT §11.5），所以 R1、R3、B11 的「最终对账 `landed: false`」判据改为 `audit_entry: absent`、`landed: null`（CONTRACT v0.9 M10）。历史 FINDING 的留证保留不动。同日补：R3T1X 的桥侧判据改为跟随重启时 `/audit` 的实际内容（此前只写了无条目一种，在修好审计的 runtime 上会误报 FAIL 而不是 FINDING）。 |

## D11 与本地 MCP 增量（2026-10-30 UTC+8）

| 用例 | 输入 | 必须观察到 |
|---|---|---|
| F6 | 真实采样，追加无有效签名的 accepted | 不为 verified；事件拒绝计数增加 |
| F7 | 真实已签 accepted，暂停真实采样并追加伪造新鲜反馈 | 不为 verified；样本拒绝计数增加 |
| F8 | 事件和样本都伪造 | 不为 verified；两个角色均计数 |
| MCP | 真实 stdio 初始化／列工具／提交／查询，随后重启 MCP 再查 | 正好两个工具；真实签名证据得到 verified；重启后仍可查询；仅一次运行时写入 |
| MCPPROD | MCP 同例，使用去掉 lab hooks 的独立包 | 同上；包内包含独立采样和验签实现 |

单元回归另覆盖错签名、跨角色、错 hint、撤销、历史密钥、重复 JSON 字段、缺密钥、伪造锚、MCP 参数与帧边界、证据缓存预算、模糊落盘失败不自动重发。五个 D11 变异都必须由断言识别。精确数量与完整故障回归结果见 `../VALIDATION-D11.md`。
