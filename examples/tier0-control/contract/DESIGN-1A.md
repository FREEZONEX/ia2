# 阶段 1a 设计与工作拆分（草案，不含代码）

2026-10-27 · 配套 [`CONTRACT.md`](CONTRACT.md)（v0.3）。WP0 已完成，结果见 [`../tier0-ia2-control-1a-20261027/RESULT-WP0.md`](../tier0-ia2-control-1a-20261027/RESULT-WP0.md)、[`TEST-CASES.md`](TEST-CASES.md)（43 个用例）。

**更新（2026-10-28）：** 这份文档 2026-10-27 起草时是纸面设计；WP0 与最小纵向切片（A1、B1、B3、C1）之后已有实现和运行，见 `tier0-ia2-control-1a-20261027/RESULT-WP0.md`、`RESULT-SLICE.md`，下文凡与它们不符之处以它们为准。没有碰 grey0，没有推送。文中「已核」指我读过具体行（本机脚本，或 `ia2` 的 `1c42df7` 对象，用 `git show` 读，没动共用检出的工作树）；没有运行过的东西一律写「读码」。

## 1 已定的事与它们对 1a 的影响

八项决定见 `CONTRACT.md` §10。对实现范围的实际影响：

| 决定 | 1a 少做 / 多做什么 |
|---|---|
| D1 不撤回 | **少做**：桥里没有撤回路径；C3 只跑一个分支 |
| D8 不补发 | **少做**：没有补发路径；R1 只跑一个分支 |
| D3 无批准层 | **多做一点**：预留字段要 fail closed（F5） |
| D6 自检 | 运行中的 runtime 不暴露北向状态，自检只能读文件（§3.6）；C10 不需要 MQTT broker，C11（表征）才需要 |
| D4 升 v2 | 读侧 `derive_action` 要**重写**（§4），不是改几行 |
| D2 HMAC、D10 精确字节 | 信封 + 先验签后解析（§3.3） |
| D9 JSON 注册表 | 零依赖解析；解析器拒绝重复键与未知字段（§3.2 `registry`） |
| D5 | 1a 在 Mac 上不涉及部署形态 |
| D7 | 标签格式固定为 `tier0.<agent>.<id8>`，字符集 `[A-Za-z0-9._-]` |

## 2 1a 的形态

```
harness（一个 asyncio 进程，沿用 rehearse.py 的做法；只信号自己起的子进程）
 ├─ 假从站 ×2–3          进程内 FakeSlavePlus（harness 直接改寄存器、切模式）
 ├─ 沙箱 IA2 runtime      子进程 127.0.0.1:13501（构建见 TEST-CASES §1）
 ├─ Tier0 替身            进程内：控制入口替身（持密钥、签名）→ requests.jsonl（追加；可注入重复 / 乱序 / 延迟 / 伪造）
 │                                              trace.jsonl（桥写）
 ├─ 桥                    子进程，测试构建 + hooks：requests.jsonl → 闸门 → HTTP → journal → trace.jsonl
 ├─ 采样器                对只读采集器代码的拷贝 + 落盘版 Archive → samples.jsonl
 └─ 读侧验证器            库函数：trace + samples → 动作终态；不 import 桥的任何代码
```

隔离规则：
- 桥和验证器**只共享一个词汇文件**（状态、原因码、字段名），不共享代码。否则验证器会替桥的错误背书（E6、F1）。
- 采样器和复用的假从站、工程生成器都是**拷贝**，原目录有哈希清单，不改；拷贝时把来源哈希记入 `SOURCES.sha256`。
- 端口、信号、构建的规则照 `TEST-CASES.md` §1，不重复。
- Tier0 替身只模拟对协议有影响的性质：追加写、至少一次投递（会重复）、可乱序、任何写入者都能追加。它**不**模拟 Tier0 的存储与查询。这些是 1b 的事。

## 3 桥参考实现

### 3.1 语言与依赖

Python ≥3.8、**仅标准库**，与 `fakemodbus.py`、`rehearse.py` 一致。Mac 上是 3.9.6，**没有 `tomllib`**；grey0 的 Python 版本我没查。

HTTP 用 `http.client`：不跟随重定向，超时可控，响应体被截断时抛 `IncompleteRead`——这正是 M9 要的严格性。`urllib` 默认跟随 3xx（`acceptance.py` 为此写过 `NoRedirect`）。主循环单进程单线程、逐请求同步处理，吞吐无要求（全桥 ≤60 次写/分），换来状态机可推演。

### 3.2 模块

| 模块 | 职责 | 纯函数 | 主要单测 |
|---|---|---|---|
| `vocab` | 状态、原因码、字段名常量 | 是 | 与 `CONTRACT` §4 对表 |
| `auth` | 信封解析、验签、密钥表（`key_id` → 密钥 + 有效期）、`hmac.compare_digest` | 是 | F2 全部变体 |
| `registry` | 加载、校验、哈希钉住、范围检查、`requires_approval` fail closed | 是 | F3、F5 |
| `values` | 类型 / 范围校验；REAL→f32 位型→有符号 i32；BOOL / INT / DINT 编码；f32 舍入后复查范围 | 是 | A2、B2 的数值向量 |
| `gates` | G1–G9，输入 (请求, 注册项, 运行视图, journal 视图, 时钟) → 通过或原因 | 是 | B 组、C1/C5/C7 的闸门部分 |
| `classify` | M1–M13 表驱动：(HTTP 结果或异常, 请求位型, 前后检查) → (状态, 原因, 附注) | 是 | B11、M 表逐行 |
| `ia2client` | `/health` `/status` `/write` `/audit`，严格解析，不跟随重定向 | 否 | 对着桩 IA2 |
| `journal` | 追加、`fsync`、哈希链、重放、去重索引、速率窗口 | 否 | B9、R1/R2 |
| `engine` | 流程（§3.5）与启动恢复 | 否 | R 组 |
| `reconcile` | 按标签在 `/audit` 里找条目 | 是 | A4、R2 |
| `northbound_guard` | D6 自检（§3.6） | 是 | C10 |
| `intake` / `trace` | 与 Tier0 替身的接缝；1b 换成真 Tier0 | 否 | — |
| `testhooks` | **单独文件**，只进测试构建（§3.7） | — | — |

「纯函数」一栏是设计目标：能写成纯函数的都写成纯函数，因为一半以上的 P0 用例（B1–B8、B11、F2、A2）因此能在不起 runtime 的情况下先过一遍。

### 3.3 请求信封与签名（D2、D10 已定）

`CONTRACT` v0.1 写的是 `sig = HMAC(规范化 JSON(其余字段))`，v0.2 已改。原因：签名方（Agent 面的控制工具集）和验签方（桥）将是两个实现，规范化 JSON 在数字格式上有歧义（`12.5` / `12.50` / `1.25e1`，`12` / `12.0`）。后果要么是合法请求被拒，要么——更糟——同一签名对应两种解释。

**所以改为对精确字节签名（已定）：**

```
{"v":"ia2.control/v0","key_id":"k1","payload":"<JSON 文本，作为字符串>","sig":"<hex>"}
sig = HMAC-SHA256(secret[key_id], b"ia2.control/v0\n" + payload.encode("utf-8"))
```

验签通过后才 `json.loads(payload)`，payload 里是 §3.1 除 `key_id` / `sig` 之外的全部字段。签的字节就是被解析的字节，没有规范化歧义。代价：Tier0 里看到的请求事件的 payload 是字符串，读侧要多解一次。

- **重放**：靠 `action_id` 去重加 G4 时窗，不另设 nonce。
- **轮换**：密钥表允许同时存在两个 `key_id`（各带 `not_before` / `not_after`）。
- **密钥保管（1a）**：0600 文件；不进 argv、环境变量、日志、journal（F2 扫描验证）。
- **占坑**：未通过验签的请求**不得写入去重表**，只记入限量的 `unauthenticated` 日志。否则任何人可以伪造一个 `action_id` 去占住真实请求的位置（F2 新增「占坑」变体）。

### 3.4 journal

追加式 JSONL，每条 `fsync` 后才继续。Mac 上 `fsync` 不保证落到盘面（要 `F_FULLFSYNC`），1a 不测掉电，只在结果里写明。

| 字段 | 内容 |
|---|---|
| `seq`、`t_ms`、`prev` | 序号、桥时钟、前一行的 sha256（哈希链） |
| `kind` | `start` · `intent` · `gate_reject` · `response` · `post_check` · `terminal` · `reconcile` · `hold` |
| `action_id`、`req_hash` | 去重键与请求哈希 |
| `state`、`reason` | 契约 §4 的状态与原因码 |
| `gate_readings` | `/health`、`/status` 的哈希与关键字段；运行实例身份；`northbound.toml` 的哈希 |
| `http` | 发出的请求与收到的完整响应（含状态码）；异常类型 |
| `label` | 发给 IA2 的 `X-IA2-Origin` |

启动时：
- 最后一行不完整 → 复制到 `journal.torn.<时间>` 后截断到最后一条有效行，并在 `start` 记录里写明；
- 中间行哈希链断裂 → **拒绝启动**；
- 去重索引、速率窗口都从 journal 重建（所以重启不会让 G6 的窗口清零）。

trace 事件由 journal 派生，`event_id` 是 `(action_id, seq)` 的确定性哈希，所以 Tier0 重复摄入同一事件（T2）无害。

### 3.5 引擎流程

1. 解信封 → 验签（G1）。失败：记 `unauthenticated`，不写去重表，结束。
2. 解 payload，G2–G5。G5 命中同内容 → **复述已记录的结果**，不写 IA2。
3. G6（读 journal 窗口）、G7（`hold` 文件）。
4. 读 `/status`、`/health`，再读 `/status` 直到 `scan_count` 前进（最多 `advance_wait_ms`，默认 600 ms；发布的 `scan_count` 约每 109 ms 才跳一步），G8、G9；比对 `northbound.toml` 哈希（§3.6）。
5. 以上任何一步拒绝 → 写 `gate_reject`（终态），写 trace，**不触达 IA2**。
6. 写 `intent` 并 `fsync`。*（R1 的钩子点在这之后）*
7. `POST /write`，HTTP 超时默认 3 s。
8. 写 `response`（含原始响应）。*（R2 的钩子点在这之后、`terminal` 之前）*
9. 发送后检查（≤500 ms）：再读 `/health`；按标签在 `/audit` 里找佐证（找不到不算失败）。
10. `classify` → 写 `terminal`、写 trace。

没有撤回、没有补发、没有自动重试。

### 3.6 北向写自检（D6）

`1c42df7` 的 runtime 里，`/health` 和 `/status` 不暴露北向状态（核对过 `main.rs`；`docs/api.md` 的 `/api/northbound` 是 IDE server 的接口，不是 edge runtime 的）。`allow_write` 在 `northbound.toml` 的 `[mqtt]` 表里，缺省 false（`types.rs` 已核）。所以桥只能读部署目录里的这个文件。

2026-10-28 D6 复查发现，逐行文本检查会漏判合法的转义键及字符串内 `#`，因此改用随桥携带的固定版本 Tomli 2.2.1 完整解析 TOML。文件不存在、没有 `mqtt` 表或省略 `mqtt.allow_write` 时按运行时缺省 false 处理；显式值必须是布尔 false，即使 `mqtt.enabled` 为 false 也不豁免。TOML 无效、`mqtt` 不是表、写权限字段类型错误或文件不可读均拒绝。启动与每次新请求使用同一检查；错误消息不包含可能泄露凭据的解析器原文。解析器源码、许可证及来源哈希一并进入生产包，不要求额外安装 Python 依赖。

触发时机：启动；`runtime_id` 变化；每次 G8 之前顺手比对文件哈希。

**局限（要写进结果）：** 它回答的是「下一次 runtime 启动会用什么配置」，不是「当前运行的 runtime 是否订阅了 `<prefix>/write`」。文件在 runtime 启动后被改而 runtime 没重启时，二者可能不一致。要关掉这个缝，需要 runtime 暴露北向状态（改 IA2，不在 v0）或阶段 2 在 grey0 上核对 broker 订阅。

### 3.7 测试钩子

- 钩子点在引擎里是命名的空调用：`after_gates`、`after_intent`、`after_response`、`before_terminal`、`skip_gate:<G8|…>`。只有测试构建会把它们接到 `testhooks`。
- **崩溃用真的 `kill -9`**：钩子到点写一个标记文件并睡眠，harness 看到标记后发 `SIGKILL`。不用桥自己 `exit`，因为那模拟不了「进程在任意指令处被杀」。
- 构建风味写进 journal 的 `start` 记录和用例留证（构建哈希）。生产构建的打包脚本不包含 `testhooks`，并断言产物里搜不到它；harness 拒绝把测试构建的结果算作生产构建的证据。

## 4 读侧验证器 v2

输入：`trace.jsonl`、采样事件、注册表的 `verify` 配置。输出：每个动作的终态、`claim` / `not_claimed`、`observation_refs`。

`acceptance.py` 的 `derive_action`（L389–419，已核）有两处**不能照搬**：

1. **起点**：它先找一条 `status == "verified"` 的终态事件，再核查那条事件引用的观测。任何能写 trace 的人都能造出这条候选事件（虽然仍须引用真实观测）。v2 必须从「`accepted` + 观测」自己推导，**忽略任何 `verified` 声明**（E6）。
2. **比较对象**：它比的是 `request['target']`（±0.01），观测源固定为 `sensor-sim` / `vision-sim`，还要求观测带 `caused_by`。真实的 IA2 反馈样本里没有 `caused_by`。v2 改为注册表指定的反馈变量加容差，因果关系靠时间窗 + 同一运行实例 + 单飞（G6）。

采样的现实约束（已核 `collector.py`）：
- 只在 `scan_count` 前进时才产生一条 sample（L91）；
- `observed_at_ms` 是「首次看见时锚定的 collector 墙钟 − 扫描时钟」的估计（L69–72、L100–102，`time_basis: anchored_scan_clock_estimate`），**不是**源的真实墙钟。

推论：(a) 反馈能分辨的最小时间是一个轮询间隔，而 IA2 自己发布的 `scan_count` 只每约 109 ms 才变一次；(b) `settle_ms` 至少取两个轮询间隔；(c) 时间窗比较要留余量，E5 的偏移用例要覆盖这个估计误差的量级。采集器的 `poll_interval_seconds` 在它的配置文件里，我没读到，WP4 要读出来写进结果。

写法：纯函数 + 表驱动向量。§7 的六条规则、E1–E7 各至少一个正例一个反例，先做成单测，再做端到端。

## 5 夹具缺口与补法

| # | 缺口 | 现状（已核） | 补法 | 规模 |
|---|---|---|---|---|
| F-1 | 写后回读（16 位、32 位） | **已满足（WP0 实测：REAL / INT / DINT 同地址回读都正常）。** `FakeSlave._handle` 的 FC06/FC16 写进 `regs`，FC03/04 读同一个 `regs`；自测有「FC06 value is stored」；`mkproject` 的 `ao1`（写，193）与 `ao1_echo`（读，193）就是这个用法 | 不改。新生成器沿用「同地址写通道 + 只读回读通道」；REAL 目标用连续两个寄存器的同样结构 | 0 |
| F-2 | 线圈（BOOL 目标） | **缺口已由 WP0 坐实（`lamp_fb` 恒 FALSE）。** FC05 只回显（L210–211），FC15 只回显（L216–217），FC01/02 恒返回全零（L204–209）。IA2 的 Modbus 客户端写线圈用 `write_single_coil`（`client.rs` L681，即 FC05） | 新文件 `fakemodbus_plus.py`：子类化 `FakeSlave`，覆盖 `_handle`——线圈字典；FC01/02 按位打包读；FC05（0xFF00 / 0x0000）；FC15 可顺手做 | S |
| F-3 | 写日志完整性 | `_describe` 对 FC16 只记第一个寄存器（L189–190） | 同一个子类覆盖 `_describe`，记录全部写入的寄存器字与线圈位。A2「REAL 读回逐位相同」要比位型 | S |
| F-4 | 写后延迟 / 冻结回读（E2、E4、C4） | 只有全局 `delay_ms`（作用于每个应答）和四种模式 | 子类加：`mirror(源, 目的, 延迟, 变换)`、`freeze(目的)` / `thaw`、`preset(地址, 值)`，由进程内定时器实现。harness 和从站同进程，直接调方法，不需要控制通道。用途：settle 边界、「回读先于请求且碰巧等于目标」、「回读卡住」 | S–M |
| F-5 | 工程生成器 | `mkproject.py` 的 `main.st` **每个扫描周期把全部 `*_ao1 := 0`**（L74、L79–80）——在该工程里对 `ao1` 的任何写入下个周期就被覆盖，这是 C6 的行为，但会毁掉 A1；`project.toml` 只有 `name` / `version`，没有 `[governance]`；通道只有 u16 / i16 | 新文件 `mkproject_ctl.py`（不改原文件）：`project.toml` 写 `[governance] write_mode = "allowlist"` 加规则（键为 `variable` / `min` / `max`，`deny_unknown_fields`，写错一个键就整个加载失败）；目标变量不被程序赋值，C6 另出一个「被赋值」变体；通道：F32 holding（`hi_lo`）、I32、I16、coil；至少 2 台设备（`require_devices`、C7）；`northbound.toml` 变体（C10）。**变量类型与通道数据类型的一致性要生成器自己保证**：`validate_iomap` 我只在 grep 里看到 kind / direction / access 的规则，没找到类型一致性检查 | M |
| F-6 | 桩 IA2（B11） | 无 | 小型 asyncio HTTP 服务，按脚本回 500 / 非 JSON / 缺键 / 缺 `value` / 302 / 空体 / 半截体后断开 / 挂起；同时记录收到的每个请求（B1 要断言「零请求」） | S–M |
| F-7 | 对账程序 | 无 | 三方对账（journal ↔ `/audit` ↔ 假从站日志），分类规则见 `CONTRACT` §8.3 | S–M |
| F-8 | 注册表静态检查（C6、F4） | 无 | 检查三件事：目标变量是否被 ST 程序每周期赋值（保守的文本扫描，**近似**，C6 的运行结果才是证据）；注册范围 ⊆ 工程 `[governance]` 规则；变量类型 ↔ 通道数据类型一致 | M |
| F-9 | MQTT broker（C11） | 这台 Mac 上 `which mosquitto emqx` 都没有 | 选项：(a) `brew install mosquitto`（改本机软件环境，要你点头）；(b) 自写最小 MQTT 3.1.1 桩（runtime 的客户端要不要 QoS1 / keepalive / PINGREQ，没核对）；(c) **推迟**；(d) 用 runtime 自己 dev-dependencies 里的 `rumqttd 0.20` 起内嵌 broker（要一个小的 Rust 辅助程序）。建议 (c)：C10 的自检不需要 broker，C11 只是表征 | 推迟 |
| F-10 | 假 ssh（A3b，P2） | 无 | 需要 IDE server 的代理路径。建议并入 1b 或舍弃 | 推迟 |
| F-11 | Tier0 替身 + 采样器 | 采集器的 `Collector.collect()` 把事件放进 `Spool`，由 `Spool.flush(remote)` 交给 `Archive`（Tier0 上传） | 替身：见 §2。采样器：拷贝 `ia2_ro/` 并换一个落盘版 `Archive`。**WP0 已核：拷贝不能原样在 Mac 上运行**——`Source` 把目标固定为 `127.0.0.1:13001`，`process_identity` 读 `/proc`。拷贝要改两处：目标地址可配；身份改用原生 `runtime_id`（`1c42df7` 有）。因此 1a 采样器与生产采集器是「同形状、不同代码」，结果文档要列差异。**仍未核：** `flush` 对 `remote` 要求的方法名，WP4 开头先读 `spool.py` / `archive.py` | M |

## 6 工作拆分

规模：S ≈ 半天以内，M ≈ 一天上下，L ≈ 两天以上。这是我自己动手的粗估，不是承诺。

| 包 | 内容 | 依赖 | 规模 | 产出 |
|---|---|---|---|---|
| **WP0** 先决核实 | ① 从 `git archive 1c42df7`（含子模块 `80a1aa8`）导出到 scratch，独立 `CARGO_TARGET_DIR`，**Mac 本机构建** `ia2-runtime`，记哈希。② 用 F-5 的最小版起一个 1 设备工程，手工 `/health` `/status` `/write` `/audit`，把真实响应存成 `golden/*.json`。③ 核下面「WP0 要回答的问题」 | 无 | M | 构建哈希、golden 样本、核实清单 |
| **WP1** 假从站增强 + 工程生成器 | F-2、F-3、F-4、F-5 | WP0 ② 起步，可与 WP2 并行 | M | `fakemodbus_plus.py`、`mkproject_ctl.py`、各自的自测 |
| **WP2** 桥的纯函数层 | `vocab` `auth` `registry` `values` `gates` `classify` `northbound_guard` `reconcile` + 单测（标准库 `unittest`） | WP0 的 golden 样本（`classify` 的输入） | M–L | 能在没有 runtime 时过的全部单测 |
| **WP3** 桥的 IO 与引擎 | `ia2client` `journal` `engine` `intake` `trace` `testhooks`、启动恢复 | WP2 | L | 可运行的桥（测试构建） |
| **WP4** 读侧：Tier0 替身 + 采样器 + 验证器 v2 | F-11；验证器纯函数 + 向量 | WP0（样本形状）；验证器本体可与 WP3 并行 | L | 替身、采样器拷贝、`derive_v2` 与向量 |
| **WP5** 桩 IA2 | F-6 | 无，可最先做 | S–M | `stubia2.py` |
| **WP6** 对账程序 | F-7 | WP3 的 journal 格式 | S–M | `reconcile_report.py` |
| **WP7** 注册表静态检查 | F-8 | WP2 的 `registry` | M | `check_registry.py` |
| **WP8** 场景运行器与留证 | 每个用例一个场景；`runs/<id>-r<n>/` 的目录与 `MANIFEST.sha256`；总表 | WP1–WP6 | L | 全部 P0/P1 能一键跑 |
| **WP9** 浸泡与结果文档 | 2 小时随机请求浸泡 + `RESULT-1A.md`（含 FINDING 回改清单） | WP8 | M | 退出条件 1–5 的证据 |
| **WP10**（可选，P2） | F-9、F-10、C11、A3b | — | — | 看你要不要 |

**建议的推进顺序：先打一条纵向最小切片，再展宽。**
WP0 → WP1（只做最小版：F-5 + 同地址回读，不含线圈）→ WP2 → WP3（只含 A1 需要的路径）→ WP4（只含 A1 的验证）→ 跑 **A1、B1、B3、C1**。这四个用例横穿全部组件，契约里有错的话最早在这里暴露；之后再补线圈、延迟、桩、对账，展宽到其余用例。WP5 与 WP0 互不依赖，可以先做。

### WP0 要回答的问题

1. `1c42df7` 在 Mac 上构建得出来吗、要多久？（10-20 彩排的 Mac 二进制是 `39a04dd` 的，没有 `1c42df7` 的记录。）
2. `/status` 有没有 `runtime_id`？`last_snapshot.vars[]` 的形状，Input 变量有没有 `input:{device,channel,stale}`？
3. `/write`：REAL 位编码往返；clamp 响应；403 / 404 / 409 的体；`undelivered_device` 键是否永远存在。
4. IA2 写 BOOL 目标时线上是 FC05 还是 FC15；REAL 是 FC16 两个寄存器，字序是否按 `word_order`。
5. 多个假从站同时挂着时 `/health.devices[]` 的形状。
6. 采集器的拷贝能否在 Python 3.9 上跑通。
7. `/audit` 条目的字段与 `result` 文本，和 `CONTRACT` §11 的「读」条目逐项核对。

### 实现状态（2026-10-28，P0 用例之后）

| 包 | 状态 |
|---|---|
| WP0 | 完成（`RESULT-WP0.md`） |
| WP1 | 完成：`lab/mkproject_ctl.py`（REAL / DINT / BOOL 目标、两台设备、被程序赋值的输出、不在治理名单里的目标）、`lab/fakemodbus_plus.py`（线圈、完整写入日志）。F-4 的「延迟 / 冻结回读」**没有做**：E1、E2 改用「暂停采样器」控制哪些样本存在 |
| WP2 | 完成（`bridge/` 纯函数层；`tests/test_pure.py`）。`reconcile` 以引擎内的 `/audit` 对账实现 |
| WP3 | 完成，**含崩溃对账**：重启时按 journal 的响应记录或 `/audit` 条目收尾，不重发；传输层 `unknown` 周期性对账成 `reconcile_note`。生产构建的钩子剥离与断言**没做** |
| WP4 | 完成：Tier0 替身、采样器（同形状、不同代码）、验证器 v2（含 `runtime_restarted` 规则）、录制代理 |
| WP5 桩 IA2 | 完成：`lab/stubia2.py`（B11） |
| WP6 对账程序 | **没做**（独立的 journal ↔ `/audit` ↔ 设备日志三方对账；各用例里有针对性的核对，但没有通用程序，所以 2 小时浸泡的「不明写入 = 0」还没法判） |
| WP7 注册表静态检查 | 部分：`lab/check_registry.py`（程序赋值、未声明、不在治理名单、范围超出）。F4 的用例本身没跑 |
| WP8 | 24 个 P0 场景 |
| WP9 浸泡 | **没做** |

跑法与结果：`RESULT-SLICE.md`（A1、B1、B3、C1）、`RESULT-P0.md`（全部 24 个 P0）。

## 7 用例 ↔ 工作包

| 用例 | 需要 | 说明 |
|---|---|---|
| B1 B2 B6 B7 B8 F2 F3 F5 | WP2 即可先做单测；端到端加 WP3、WP5（桩，断言零请求） | 不需要 runtime |
| A2 | WP2 向量 + WP1（F-3 位型） | |
| B11 | WP5 + WP3 | |
| A1 B3 B4 B5 C1 C5 C7 | WP0 WP1 WP3 WP4 | 真 runtime；A1 是最小切片 |
| C3 C4 | 同上 + 假从站模式切换（已有） | 数据是交付物 |
| C6 | 同上 + WP7 | 「被赋值」工程变体 |
| C8 | 同上 + kill / 重启沙箱 | |
| C10 | WP2（自检） | 不需要 broker |
| R1 R2 R3 B9 | WP3 的钩子 + WP8 的 kill / SIGSTOP | |
| E1–E7 | WP4 的验证器；E1/E7 的端到端要 WP1 F-4 | 先向量后端到端 |
| A4 | WP6 | 300 次写 |
| F1 | **1a 里只能做一半**：桩出的「控制入口」工具清单 + 只读 MCP 的 `delivery-lock.json` 哈希不变。真正的 Agent 面工具集 1a 还不存在 | 见 §8 第 7 条 |
| F4 | WP7 | |
| C9 | 另需 #98 头 `5ca976e` 的构建 | 先读该 PR 头确认字段名 |
| C11 A3b | WP10 | P2 |

## 8 风险与不确定

1. ~~`1c42df7` 在 Mac 上可能构不出来。~~ **已解除**：WP0 构建成功，1 分 34 秒。
2. **Python 版本。** Mac 3.9.6 无 `tomllib`；grey0 的版本未查。注册表因此用 JSON（D9 已定）。
3. **回读等于「设备寄存器收下了」，不是外部传感器。** 同地址回读（F-1）证明的是模拟设备的寄存器值，对应契约的 L2 证据。分离式回读（F-4）才接近「传感器看到了」，也仍是模拟。结果文档里必须写明。
4. **采样粒度与时间锚。** 见 §4：反馈最小分辨约一个轮询间隔，`observed_at_ms` 是估计值。E4 / E5 的边界用例要按这个现实设计，否则会得出「边界不准」的假 FINDING。
5. **`derive_action` 不可复用** 的两处（§4）；容易低估这块工作量。
6. **IA2 是否校验变量类型 ↔ 通道数据类型** 未知（grep 没找到）。生成器和静态检查必须自己保证；若 IA2 不校验，类型错配的工程会静默截断数值。
7. **「控制工具集」（Agent 面的 MCP 或命令行）放哪、谁持有签名密钥，没有定。** 1a 里由 Tier0 替身签名。1b 之前必须定，F1 才有真对象可测。这不阻塞 1a。
8. **C3 的结论可能反过来冲击 D1。** 若迟到命令能在 deadline 之后很久才执行且值仍有危险，要复议「不撤回」。
9. **共用检出。** 任何构建、核对都用 `git archive` 导出到 scratch；不在 `~/Projects/soft-PLC/ia2` 里切分支、构建或跑闸门。
10. **掉电语义不测。** `fsync` 在 Mac 上不保证落盘；1a 只测进程被杀。

## 9 已确认的小事

| # | 问题 | 决定（2026-10-27 用户） |
|---|---|---|
| D9 | 注册表文件格式 | **JSON**；`CONTRACT` §3.2 已改成 JSON 字段说明 |
| D10 | 签名对象 | **精确字节**（信封 + `payload` 字符串，先验签后解析）；`CONTRACT` §3.1 已改 |

仍不用答、但你应当知道：F-9（MQTT broker）建议推迟。补充一个我在 WP0 里发现的选项：`ia2-runtime` 自己的 dev-dependencies 里有 `rumqttd`（一个内嵌 MQTT broker 库），将来做 C11 不必装 mosquitto，可以拿它起一个本机 broker（要一个小的 Rust 辅助程序，规模 S–M）。

## 10 变更记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0 草案 | 2026-10-27 | 首版。读码与读本机脚本，无任何运行。 |
| v0.1 | 2026-10-27 | D9、D10 已定（§3.3、§9）；F-9 增选项 (d)。WP0 开始。 |
| v0.2 | 2026-10-27 | WP0 完成：F-1、F-2、F-11 与风险 1 据实修订。 |
| v0.3 | 2026-10-28 | 切片完成：加「实现状态」；G8 改为等 `scan_count` 前进；采样粒度的推论补上快照节拍。 |
| v0.4 | 2026-10-28 | P0 完成后：实现状态改写；F-4 未做、WP6 / WP9 未做、桩 IA2 与崩溃对账已做。 |
| v0.5 | 2026-10-28 | 复核前的畸形输入探针与修复（`safe_handle_line`、纯 ASCII 落盘）；复核任务书 `REVIEW-BRIEF.md`。 |
| v0.6 | 2026-10-28 | 修复 W1–W19（`RESULT-FIXES.md`）：验证改在扫描时钟里；trace 锚；enable 文件；生产打包；预检；治理检查；行长上限；对账两次；单飞窗口；24 小时索引。实现状态表里「WP3」「WP4」按此理解。 |
