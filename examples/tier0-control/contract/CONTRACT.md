# ia2.control/v0 — Tier0 ↔ IA2 控制写入联调契约（阶段 0 草案）

状态：**草案 v0.11，2026-10-29**（v0.9 只改 M10 对账记录的措辞，见 §11.5；v0.10 记录对账程序与 P1、浸泡的实测，见 §8.3、§11.6、§12；v0.11 按审核修复对账判据，见 §8.3、§12；D1–D10 已于 2026-10-27 定下，D11 已于 2026-10-28 定下，见 §10；D12–D14 同日定下；D11–D14 只有设计，代码尚未实现，见 `DESIGN-D11-signed-events.md`）。2026-10-27 起草时是纸面设计；之后 WP0 在 Mac + 假从站上实测了若干条 IA2 行为（§11.1），24 个 P0 用例已在桥参考实现上全部运行（§11.2、§11.3；`tier0-ia2-control-1a-20261027/RESULT-SLICE.md`、`RESULT-P0.md`）。实现等待独立复核：`tier0-ia2-control-1a-20261027/REVIEW-BRIEF.md`。上述为原独立副本的历史记录；仓库移植及本轮验证见上级 README 与 VALIDATION。

IA2 的行为除 §11.1 列出的几条已测外，都是**读 `origin/main`（`1c42df7`）代码和文档得出的预期**，没有在运行实例上验证过。阶段 1 的每个用例（`TEST-CASES.md`）要么确认它，要么把它变成 FINDING 并回来改本契约。§11 列出每条依赖及其把握程度。

## 1 范围

- **目标**：Agent 经 Tier0 提交"把某个已登记目标设为某值"的请求；由一个独立的桥执行；结果如实回报，包括"没生效"和"不知道"。
- **覆盖**：电平型设定值写入，类型 REAL / DINT / INT / BOOL。
- **不覆盖**：§9。
- 约定：**必须**＝不满足即视为契约违反；**应当**＝有充分理由才可偏离。

## 2 组件与路径

```
Agent ──控制工具集（独立入口；既有只读 MCP 的 7 个工具不变）──► Tier0  Action/requests（已签名）
                                                                  │
                                    ia2-control-bridge（grey0 宿主机、独立用户服务；本契约的被测物）
                                                                  │ 只发 HTTP，127.0.0.1:<runtime 端口>
                                                                  ▼
                                    IA2 edge runtime   /health  /status  /write  /audit
Tier0 ◄── Action/trace      由桥写：accepted / rejected / modified / undelivered / unknown
Tier0 ◄── 既有只读采集器    samples：变量值、input.stale、device_health、watchdog_tripped、forces；并归档 /audit
Agent ◄── query_action_results   读侧；verified 只在这里由证据派生，桥不写
```

### 设计原则

| # | 原则 |
|---|---|
| P1 | 桥是**唯一**写 IA2 的组件。Tier0 容器、Agent、既有采集器都不写。沿用 ADR-0002 决策 3：sidecar 优先、runtime 零改动、runtime 继续只绑 `127.0.0.1`（`DEFAULT_BIND`），远程靠 SSH。 |
| P2 | **双层许可**：桥注册表（人审、哈希钉住）∩ IA2 `[governance]`（allowlist + min/max）。任一层拒绝即不生效；注册表范围必须 ⊆ 治理范围。 |
| P3 | Agent 只给**注册表键名**和数值，永远不提供 IA2 变量名或位编码值。 |
| P4 | **已接受 ≠ 已生效 ≠ 已验证。** 桥只能写 accepted / rejected / modified / undelivered / unknown；`verified` 永远由读侧从独立观测证据派生，桥必须不写。 |
| P5 | **缺证据 ＝ 不成功。** 没有反馈通道的目标，状态最高到 accepted。 |
| P6 | **写前意图落盘**，崩溃后可对账；去重**持久化**（10-22 的 Tier0 验收只做了进程内去重）。 |
| P7 | 失败**默认不重试、不撤回、不回滚**；任何自动动作（撤回、重发）必须是 §10 里用户明确授权过的。这不是"所有自动恢复一概禁止"：预先授权且经验证的自动动作可以成立，只是 v0 没有被授权的。 |
| P8 | 桥不扩大权限：不 force/unforce、不 pause/resume/step/stop、不注入故障、不确认报警、不部署、不改配置、不用 MQTT 写通道。 |

## 3 请求与注册表

### 3.1 请求（Tier0 `Action/requests` 事件）

字段命名沿用 `ia2.observation/v1` 的 `session_id`（`tier0-acceptance` 脚本里的 `run_id` 同义）。

外层是**信封**，签名只覆盖 `payload` 的原始字节（D10）：

```json
{ "v": "ia2.control/v0", "key_id": "k1", "payload": "<下面这个对象的 JSON 文本，作为字符串>", "sig": "<HMAC-SHA256 十六进制>" }
```

`payload` 解开后的对象：

```json
{
  "event_id": "…", "action_id": "<128 位随机十六进制，全局唯一>", "session_id": "…",
  "device_id": "bench01-sim",
  "command": "ia2_set",
  "target_key": "sp_flow",
  "target": 12.5,
  "simulation": true,
  "requested_at_ms": 1790000000000, "deadline_ms": 1790000020000,
  "requested_by": "agent:<id>"
}
```

- `command` 在 v0 **只有** `ia2_set`。`target` 是 JSON 数字或布尔。`simulation` **必填**。
- **签名**：`sig = HMAC-SHA256(secret[key_id], b"ia2.control/v0\n" + payload 的 UTF-8 字节)`。桥**先验签、后解析**：签的字节就是被解析的字节，没有规范化歧义（数字写法、键序、空白都无关）。验签通过但 `payload` 不是合法 JSON、含重复键或缺字段 → `rejected bad_request`。密钥只存在于控制工具集所在主机和桥所在主机，**不进入 Tier0**。
- **为什么要签名**：Tier0 3.0.0 在 grey0 上没有逐主题密钥 ACL（`tier0-acceptance-20261022/RESULT.md`："尚无逐主题密钥 ACL"），持有实例级 API 密钥的任何组件都能写 Action 主题。所以 **Tier0 主题里的内容只是传输，不是认证**。（D2 已定为 HMAC 签名、D10 已定签名对象为精确字节，见 §10 与 `DESIGN-1A.md` §3.3。）

### 3.2 注册表（桥侧，人审，哈希钉住）

注册表文件是 **JSON**（D9）：文件 sha256 钉的是字节；人读靠每项的 `note` 字段。下面是一项的字段说明（实际文件是这些对象组成的数组，外面套 `{"version":1,"targets":[…]}`）：

```json
{
  "key": "sp_flow",                    // Agent 唯一能用的名字
  "device_id": "bench01-sim",
  "ia2_variable": "sp_flow",           // 必须与 /write 接受的名字逐字相同（区分大小写）：单实例工程是快照里的裸名，多实例是 <实例名>.<变量>，实例名取自 tasks.toml 的 instance，不是程序名；Agent 永远看不到
  "type": "REAL",                      // REAL | DINT | INT | BOOL；LREAL 在 v0 不支持（见 §9）
  "min": 0.0,
  "max": 50.0,                         // 必须 ⊆ 部署工程 [governance] 对该变量的 min/max
  "simulation": true,                  // 与请求的 simulation 必须一致
  "enabled": true,
  "requires_approval": false,          // v0 没有批准层：enabled 且为 true 的项 ⇒ 桥拒绝启动（D3、F5）
  "require_devices": ["coupler"],      // 发送时必须 healthy 的设备
  "require_fieldbus_healthy": true,
  "max_rate_per_min": 6,
  "min_interval_ms": 2000,
  "max_deadline_ms": 60000,
  "single_flight_ms": 5000,            // 一个 accepted 动作让目标保持「忙」多久；默认 = verify.settle_ms，无反馈通道 2000
  "verify": { "feedback": "ft0202",    // 由 Input 通道映射的 IA2 变量（才有 input.stale）；省略 ⇒ 永远不 verified
              "tolerance": 0.2, "settle_ms": 5000 },
  "note": "给审阅的人看的说明"
}
```
（上面的 `//` 注释只是说明，真实文件里不能有注释；解析器拒绝重复键和未知字段。）

- 注册表文件的 sha256 **必须**写进桥的启动配置；不符则桥拒绝启动。
- 桥启动时**应当**检查注册表范围 ⊆ 部署工程的治理规则（我读到的文档里没有能从运行中的 runtime 取治理规则的接口，所以这是部署期的离线检查；见 F4）。
- 注册表**应当**经静态检查确认 `ia2_variable` 不被程序每个周期赋值（否则写入下一周期就被覆盖，见 M5 与 C6）。

## 4 状态词汇

| 状态 | 终态 | 含义 | 谁写 |
|---|---|---|---|
| `requested` | 否 | 已收到，尚未判定 | 读侧派生（有请求、无桥事件时） |
| `accepted` | 否 | IA2 接受了写入且当时传输健康；**未证明生效** | 桥 |
| `verified` | 是 | 反馈通道在期限内报告目标值（容差内），新鲜且属于同一运行实例；**未证明线上送达、未证明物理效果** | **读侧派生** |
| `unknown` | 是 | 无法确定；必带原因 | 桥或读侧 |
| `rejected` *(新)* | 是 | 没有写入（桥拒绝，或 IA2 明确拒绝）；状态未变 | 桥 |
| `modified` *(新)* | 是 | 已写入，但值与请求不同（IA2 clamp）；永不算成功 | 桥 |
| `undelivered` *(新)* | 是 | IA2 已把值放进 VM，但报告目标设备不可达；值可能在链路恢复后被迟到执行 | 桥 |

- 与 `ia2.observation/v1` 的关系：v1 的 `query_action_results` 状态固定为 requested / accepted / verified / unknown。新增三个终态要读侧合同升到 v2（D4）。**v1 消费者遇到未知状态必须当作"非成功"。**
- 终态之后**不再升级**：`undelivered`/`modified` 之后即使反馈碰巧等于目标，也不变成 `verified`；v1 的规则也是证明必须落在请求到截止之间。

**原因码**（有限集合）：`bad_request` `unauthenticated` `not_in_registry` `simulation_mismatch` `unsupported_type` `bad_value` `out_of_range` `expired` `conflicting_action_id` `rate_limited` `busy` `held` `runtime_unreachable` `latched` `paused` `forced` `device_unhealthy` `fieldbus_unhealthy` `governance_denied` `unknown_variable` `scan_stopped` `vm_error` `clamped` `link_down` `latched_after_write` `runtime_restarted` `no_feedback` `feedback_stale` `no_proof_by_deadline` `northbound_write_enabled` `bridge_restarted`

### 面向 Agent 的措辞（必须随结果返回）

每条结果带 `claim` 和 `not_claimed`，Agent 复述时不得扩大：

| 状态 | claim | not_claimed |
|---|---|---|
| accepted | IA2 接受了写入，传输当时健康 | 生效、线上送达、物理效果 |
| verified | 反馈通道在期限内报告了目标值（容差内），数据新鲜 | 线上送达、物理效果、反馈通道本身没有被卡住 |
| modified | 已写入 `applied_value`，不是请求的值 | 请求被满足 |
| undelivered | IA2 接受但设备不可达；值可能迟到执行 | 任何生效 |
| rejected | 未写入 | — |
| unknown | 不知道（见原因） | 成功或失败 |

## 5 发送前闸门与发送后检查

闸门按序执行，第一个失败即终止，状态 `rejected`（原因见括号）。**被拒的请求不得触达 IA2**（IA2 的 `/audit` 在该时段不应出现对应条目）。**例外的顺序：G5（去重）实际紧跟 G1 之后、在 G2 之前执行**——已执行动作的重放必须复述已记录的结果，否则会得到一条新的 `expired` 而覆盖 Agent 已看到的状态（2026-10-28 实现时发现）。

| 闸门 | 检查 |
|---|---|
| G1 格式与身份 | JSON 合法、字段齐全且**无未知字段**、类型对；`command` 必须是 `ia2_set`（其他一律 `bad_request`，detail `unsupported_command`）；`key_id` 在册且 `sig` 有效（`bad_request` / `unauthenticated`）。**信封畸形、验签失败、payload 不是严格 JSON（`NaN` / `Infinity` 字面量、重复键）都没有可信的 `action_id`，只记入 journal，不产生逐动作的 trace 事件，也不进入去重表** |
| G2 注册表 | `target_key` 在册且 `enabled`；`command == ia2_set`；`simulation` 与注册项一致（`not_in_registry` / `simulation_mismatch`） |
| G3 值 | 有限；类型匹配；在 `[min, max]`；REAL 转 f32 后仍在范围内；BOOL ∈ {0,1,false,true}；`-0.0` 与 `1e999` 之类解析成 ±∞ 的数一律 `bad_value`（`bad_value` / `out_of_range`）。注册表里出现非支持类型（LREAL 等）时**注册表加载即被拒、桥不启动**，所以 `unsupported_type` 在 v0 里到不了请求路径 |
| G4 时间 | `deadline_ms` 在未来（按处理时刻）且 ≤ `max_deadline_ms`；`requested_at_ms` 与**这一行到达桥的时刻**相差 ≤ 2 s（`expired`）。按到达时刻而不是处理时刻，桥自己排队的时间不算「过期」 |
| G5 去重 | 查 journal：无 → 继续；有且请求哈希相同 → **返回已记录的结果，不再写**；有但内容不同 → `conflicting_action_id`，以一条 `conflict_notice` 事件回报，**不改变原动作的状态**。**之后**：标签（D7 的 `tier0.<agent>.<id8>`）已被另一个动作使用 → `conflicting_action_id`（detail 说明是标签冲突），发送前拒绝；换一个 `action_id` 重发即可 |
| G6 节流 | 全桥 ≤ 60 次写/分；目标的 `max_rate_per_min`、`min_interval_ms`；**单飞**：同一目标有未终结动作时拒 `busy`，不替换（未终结＝已发出未结论，或 `accepted` 且仍在 `min(deadline_ms, single_flight_ms)` 之内；`single_flight_ms` 是注册表字段，默认取 `verify.settle_ms`，无反馈通道为 2000，之后读侧已有结论）（`rate_limited` / `busy`） |
| G7 急停开关 | `hold` 文件存在 → `held`。`hold` 只拒新请求，**不撤销**已发出的写 |
| G8 IA2 状态 | 读 `/status`、`/health`，再读 `/status` 直到 `scan_count` 前进，最多等 `advance_wait_ms`（默认 600 ms）（任一读失败 → `runtime_unreachable`）：`status == "ok"` 且 `fault == null`；`watchdog_tripped == false` 且 `latch_reason == null`；`snapshot_tap.running == true`；`scan_count` 在该时限内前进（IA2 发布的 `scan_count` 约每 109 ms 才跳一步、一步 10–11 次扫描，不是每次扫描都变——2026-10-28 实测，见 §11.1；固定间隔读两次会随机误拒）；**`failsafe_status` 扩展**：旧运行时可以省略；当前运行时在尚无尝试时返回 null；非 null 必须是恰含 `pending`、`acknowledged`、`unconfirmed` 三个列表的对象。格式错误 → `runtime_unreachable`；任一列表非空 → `latched`。null 不构成安全证明，也不覆盖另一份快照或 watchdog、锁存、故障、设备健康等独立检查。三个发送前快照分别检查。记下必需的 `runtime_id`；缺失时拒绝，不回退到其它身份组合 |
| G9 目标状态 | `mode` 为运行中（`paused`）；目标不在 `forces` 里（`forced`）；`require_devices` 全部 `healthy`（`device_unhealthy`）；若 `require_fieldbus_healthy` 则 `fieldbus_healthy == true`（`fieldbus_unhealthy`） |

**发送后检查**（读 `/status`、`/health`、`/audit`，每次读 2 s；写本身 3 s）：解析响应（见 §6）；再读一次 `/health`——仍未锁存、实例身份未变；按标签在 `/audit` 里找对应条目作**佐证**（找不到不算失败，见 §8）。

**每次 HTTP 操作只有一个总预算**（P1，F10）：从连接开始到响应正文的最后一个字节，按**单调时钟**计，读 2 s、写 3 s；连接、发送、响应头、正文和分块元数据内部的每一次底层读写都只拿到**剩下的**时间，所以对端一次只回一个字节也拖不过这个预算（只设每次读的 socket 超时做不到这一点）。这是**每次操作**的预算，不是整个动作的总时限（发送前最多是几次读的预算之和加 G8 的等待，发送后再加检查的读），也不承诺硬实时。预算用尽只说明结果不确定：关闭连接、结束这次工作、**不重试**——写不会发第二次，关连接也不等于撤回设备动作。大小也在字节到达时就受限，各有各的上限：状态行与响应头合计 16 KiB，分块的长度行（含扩展）与 trailer 合计 16 KiB，正文 2 MiB；缓存不会超过上限一个读块。刚好在预算末刻完成的操作算完成，预算用尽之后还要做的任何操作都算超时。

**响应正文边界必须唯一**（IR01，独立修复副本）：对需要读取正文的响应，读取全部 `Content-Length` 与 `Transfer-Encoding` 字段，字段名大小写不敏感。重复的长度头（即使值相同）、重复的编码头、两种头同时出现、长度的逗号列表、合并后不支持的传输编码，以及解析器报告的畸形响应头，一律是协议错误；不能只选择第一个值。支持单个非负十进制长度、单个 `chunked`，或无上述字段时读到关闭。已发送动作遇协议错误为 `unknown`，不重试、不重发；不以该响应作为 `verified` 的前提。

**原始头块不得静默丢失字段**（AR01，独立修复副本）：状态行与字段行接受 CRLF 或 LF，允许混用；外层读取和内部解析使用同一空行边界。完整头块中未跟随 LF 的裸 CR 是协议错误，在解析前拒绝；邮件头解析器的缺陷列表为空仍不足以接受响应，解析结果不得残留 payload，也不得提取出 HTTP 不存在的 Unix mailbox `From ` 分隔行。此检查覆盖临时响应和无正文响应的头块，只检查头块，不把正文中的 CR／LF 当成字段。未完整收到头块时仍按现有截断／超时规则处理。后续长度／编码检查及“已发送动作 unknown、累计一次 POST”的规则不变。

## 6 IA2 响应 → 状态映射

桥用 `POST /write`，body `{name, value, pulse_ms?}`；`value` 是**位编码的 i32**（REAL＝f32 位型，BOOL/整型＝整数），由调用方按变量类型编码。v0 **不用 `pulse_ms`**。响应成功体键为 `ok, name, value, undelivered_device`。

| # | 观测 | 状态 / 原因 | 用例 |
|---|---|---|---|
| M1 | 200，`value` 等于请求，`undelivered_device` 为 null（键存在），前后检查干净 | `accepted` | A1 |
| M2 | 200，`value` ≠ 请求（IA2 clamp；审计里是 `clamped`） | `modified`，附 `applied_value` | B4 |
| M3 | 200，`undelivered_device` 非空 | `undelivered`（原因 `link_down`）；**不撤回**（D1 已定 (a)），只告警。WP0 实测：链路恢复后值确实迟到下发，且**迟到下发不进 `/audit`** | C3 |
| M4 | 200，但发送后检查发现已锁存 / `failsafe_status` 非空 / 实例已变 | `unknown`（`latched_after_write`） | C2 |
| M5 | 200，各项干净，之后**没有**匹配的反馈 | 停在 `accepted`，到截止 → `unknown`（`no_proof_by_deadline`；无反馈通道则 `no_feedback`）。这一行覆盖：静默链路的未翻转窗口、被程序覆盖的输出、被 force 的变量 | C4, C6, E3 |
| M6 | 403（治理拒绝；审计里有拒绝条目） | `rejected`（`governance_denied`） | B3 |
| M7 | 404 | `rejected`（`unknown_variable`）；该注册项**必须**自动禁用直到人审 | B5 |
| M8 | 409（扫描循环已停） | `rejected`（`scan_stopped`） | B5 |
| M9 | 500、非 JSON、缺 `undelivered_device` 键、3xx、截断、非预期结构 | `unknown`（`vm_error` 或 `runtime_unreachable`）；**缺键不是 null**（旧版 runtime 可能没有该字段，不能当"已送达"） | B11 |
| M10 | 连接失败 / 超时 / 连接被重置（不知道有没有落地） | `unknown`（`runtime_unreachable`）；事后用 `/audit` 对账（§8.3，**看两次**）；**不在同一动作里盲重发**。桥随后（周期性，或重启时）对账，记 `reconcile_note`，**不改状态**：`audit_entry` 写 `/audit` 里看到什么（`found` / `absent` / `unavailable`＝实例已重启、环已清空）；只有 `found` 才是断言（`landed: true`），其余 `landed` 为 null，**从不写 `landed: false`**——R3T1/R3T1X 实测：已读入并排队的写在连接关闭后照样执行，却没有审计条目（§11.5）。v0.9 之前的桥写的 `landed: false` 一律按 `absent` 读 | R3、R3T1、R3T1K、R3T1X、R3T2 |
| M11 | 动作期间运行实例身份变了（重启） | `unknown`（`runtime_restarted`）；不重发；新实例上的匹配反馈**不**算该动作的证明。由读侧推导：发送之后出现另一个 `runtime_id` 的样本即 `unknown runtime_restarted`。实测：新实例启动时会把设备输出写成 0，所以「设备保留了旧值」这个陷阱要人为设置（C8 就是这么做的） | C8 |
| M12 | 闸门 G8/G9 失败 | `rejected`（`latched` / `paused` / `forced` / `device_unhealthy` / `fieldbus_unhealthy` / `runtime_unreachable`） | C1, C5, C7 |
| M13 | 闸门 G1–G7 失败 | `rejected`（见 §5 原因） | B1, B2, B6–B10, F2 |

## 7 `verified` 的条件

`verified` 由读侧派生，**全部**满足才成立（在 `derive_action` 的既有规则上收紧并泛化）：

0. **取值只信桥的事件：** 目标值、deadline、设备、会话取自桥写出的 `accepted` 事件，不取自 `Action/requests` 里的请求记录（后者在不可信的主题上）；请求记录只用来认出是哪个动作。桥的第一条事件为准（终态不升级），之后矛盾的事件被忽略。读侧可校验事件里的 `registry_sha256` 与自己持有的注册表一致。
1. 动作状态曾为 `accepted`（M1），不是 `modified` / `undelivered` / `rejected`。
2. 存在一条来自**注册表 `verify.feedback`** 的观测——它是 IA2 的 **Input 通道映射变量**，且该样本里 `input.stale == false`、所属设备 `healthy`。**读回被写的那个 VM 变量不算证据**（它只证明 VM 收下了）。
3. **顺序在运行时自己的扫描时钟里判断**：桥在写入被回答后的第一次 `/status` 读取里记 `post_scan_us`（该读取里 `last_snapshot.timestamp_us`）和 `scan_anchor_ms`（桥的墙钟减去那个扫描时间）。样本的 `scan_timestamp_us` 必须 **严格晚于 `post_scan_us`**，且不晚于 `min(post_scan_us + settle_ms, (deadline_ms − scan_anchor_ms) 换算成扫描时间)`。采样器的墙钟估计（`observed_at_ms`）**不参与**判断：采样器在别的主机、时钟偏差，也不会让旧观测显得新。快照约每 109 ms 才发布一次，标记因此可能比写入早至多一个周期；锚的误差只会让窗口变窄。IA2 的样本没有质量字段，质量由 `input.stale == false` 与设备 `healthy` 体现。
4. 同一份样本记录里 `watchdog_tripped == false`、`fault == null`、目标不在 `forces`；样本属于**同一运行实例**。
5. 样本的扫描时间最多比评估时刻（经锚换算）晚 500 ms。
6. 仅有 accepted / trace 事件，哪怕写着 `verified`，也**不算**：读侧从观测重建，忽略任何由桥或他人写入的 `verified` 声明（E6）。
7. **`verified` 不证明这次写入是原因。** 若反馈在请求之后已经等于目标（设备本来就在这个值，例如前一次迟到下发留下的），同样 `verified`（C4 实测遇到）。`not_claimed` 里写明了这一点。
8. **比较是精确的绝对差，由注册表的类型决定**（P1，F05/F06；向量见 `tests/test_p1_numeric.py` 的表）。`|反馈 − 请求| ≤ verify.tolerance`，含边界，用有理数精确运算：整数不经过浮点；浮点数取其**十进制往返表示**（能读回同一个浮点数的最短十进制文本，即保存的 JSON 所写的那个数），所以 `12.55` 对 `12.5 ± 0.05` 恰在边界上（算 `verified`），而实际 f32 位型解码出的 `12.550000190734863` 在界外。没有随数值大小增长的附加余量，也不用 epsilon 或四舍五入放宽。类型取自**受信注册表的 `type`**（不看样本自称的类型；没有可用类型即不构成证明）：`BOOL` 归一化为 0/1 后必须相等，容差不参与，相反值在任何容差下都不是证明；`INT` / `DINT` 的请求必须是整数，反馈里的非整数值照原样参与比较、不被截断；`REAL` 的请求和反馈是有限数。反馈值不是该类型的有限数（NaN、±∞、文本、null、把 bool 当数、把数当 BOOL）**不是观测**：该样本被跳过，既不会被改成 0，也不会终止对后面样本的查找（窗口内另有合法样本仍可 `verified`，证据引用指向合法样本）；窗口与宽限期之后仍无合法证明则 `unknown`（`no_proof_by_deadline`，`detail` 写明有几条无效样本）。请求值或容差本身不是可用的数（NaN / ±∞ / 类型不符 / 容差为负）则直接 `unknown`（`no_proof_by_deadline`，原因写在 `detail`），不等到截止。

证据等级沿用 `ia2-ack-vs-effect-audit` 的 L1–L4：`verified` 最多是 **L2**（运行时状态＋反馈通道）。线上字节（L3）和物理效果（L4）只在台架用抓包/仪表获得，**不属于本契约**；所以措辞里永远带 `not_claimed`。

## 8 身份、审计、持久化

### 8.1 标签

- 桥发 `X-IA2-Origin: tier0.<agent>.<action_id 前 8 位>`（D7）。字符集**只用** `[A-Za-z0-9._-]`、总长 ≤ 64：IDE server 代理会清洗成这个字符集，直连 edge runtime 的 `origin_of` 则原样取非空头——用同一个合规字符集两条路径行为一致。
- 这个标签是**自称**，IA2 文档明说"a label anyone who reaches the socket can claim"。它是对账线索，不是认证。认证靠 §3.1 的签名。

### 8.2 桥的 journal（主审计）

追加式，每条 `fsync` 后才继续。**发送 IA2 请求之前**必须已经写入"意图"条目。字段：`action_id`、请求哈希、状态与原因、时间戳、闸门读数摘要（`/health` `/status` 的哈希与关键字段）、运行实例身份、发出的 HTTP 请求与收到的完整响应（含状态码）、标签、`/audit` 对账结果。

崩溃恢复：

| 日志停在 | 恢复动作 |
|---|---|
| 有意图、无 IA2 响应记录 | 桥重启时：读 `/status`；按标签查 `/audit`。**有条目** → 按条目的 `result` / `applied` 补记 accepted / modified / undelivered / rejected（实例身份变了则 `unknown runtime_restarted`，已锁存则 `unknown latched_after_write`），**不重发**；**无条目** → `unknown`（`bridge_restarted`，实例变了则 `runtime_restarted`），**不补发**（D8 已定 (a)）。由 `/audit` 单独得出的结论没有 `sent_at_ms`，但带**恢复时刻的扫描时钟标记**（写入发生在这次读取之前、同一个运行实例——身份核对过——所以它是可靠的下界，只会让读侧更严）；读侧只接受该标记之后的反馈。用例 R1、R2(A) |
| 有 IA2 响应记录、无终态 | 用 journal 里**随响应一并落盘的分类事实**（见下）得出写入的结论；若是 `accepted`，再应用 journal 里**已有的 `post_check` 记录**（见下），然后才读 `/status` 核对实例身份与锁存——恢复时的读数只能**增加**限制，不能抹掉已记录的限制；保留 `sent_at_ms`，扫描时钟标记取恢复时刻，之后可被读侧验证。用例 R2(B) |

**落盘什么、重启信什么**（P1，F08/F09）：
- `response` 记录里的 `http.text` 只是**给人看的诊断副本**，在 4096 个字符处截断（`text_truncated`）。重启**不得**从这份副本重新解析来下结论——完整响应可能无法解析，而前 4096 个字符恰好可以。桥在收到完整响应后、写 `response` 记录**之前**就用完整响应分类，把带版本的 `facts`（版本、`action_id`、标签、变量、`bits`、传输结果、完整正文的字符数与哈希、分类结果）写进**同一条记录**，所以不存在"响应已存、事实未存"的空隙。重启只用 `facts`，并核对它属于这个动作（`action_id`、标签、变量、`bits` 与意图一致）、版本已知、状态与原因在词汇表内、必要字段齐全；任何一项不成立 → `unknown`（`vm_error`），**不回退到副本**。
- P0 及更早写的 `response` 记录没有 `facts`：传输错误不需要正文；有正文的，仅当记录明确写着 `text_truncated` 是布尔值 `false`（副本就是全文）才重新分类；被截断、缺标记、标记类型不对的副本不构成证据 → `unknown`（`vm_error`）。
- `post_check` 记录带 `v`（版本）、`label`、`runtime_id`，并且**只有完整的记录才算一次干净的检查**：`read_failed` 为布尔；未失败时 `identity_changed`、`latched_after` 为布尔，`scan` 带整数 `ts_us` 与 `anchor_ms`。恢复时对 `accepted` 的写入用与现场**同一条规则**（读失败 → `runtime_unreachable`；实例变了 → `runtime_restarted`；已锁存 → `latched_after_write`；无扫描标记 → `runtime_unreachable`）：记录里的否定事实优先于恢复时更健康的读数。不完整、版本未知、不属于这个动作、同一动作有多条、或标明"跳过"的记录 → `unknown`（`runtime_unreachable`）。没有 `post_check` 记录意味着检查没有落盘，只按响应事实与恢复时的读数判断（与上表一致）。`modified` / `undelivered` / `rejected` 等明确的响应结论不受 `post_check` 影响；已有终态的动作不会被重新分类。

**未预期的异常**（实现要求，v0.6）：处理一行请求时若引擎抛出任何未预期异常，桥记一条 `internal_error`（含该行的哈希），把这一行**按哈希隔离**——重启后桥会从头重读请求文件，不隔离就会被同一行反复击倒——并对此刻在途的动作按上表收尾（不重发）。已记过的未验签行、非严格 JSON 行同样只记一次，重启重读时不再重复记。journal 与 trace 在磁盘上是纯 ASCII（`ensure_ascii`），所以任何字符串都能写进去。

**异常收尾本身不能再击倒桥**（P1，F07）：上面"按上表收尾"要读 IA2，读本身也可能抛异常（客户端出错、响应形状之外的意外）。所以收尾**按动作隔离**：某个动作的收尾抛了异常，桥记一次内部错误计数，然后**只用 journal 里已有的事实、不再访问网络、不发送任何东西**，把这个动作结束在 `unknown`（`runtime_unreachable`，`detail` 写明"concluded from the journal alone"）——不递归恢复、不补发、不把不确定的动作说成未写。这一条对启动时的恢复同样适用，否则同一个失败会在每次重启时重演（崩溃循环）；若连这一步也做不到（journal 里的记录不是本桥写的格式），该动作保持在途：它的目标保持忙（失败即关闭），状态文件的 `in_flight` 与 `internal_errors` 能看到它。周期对账同理：某个动作的这次查看抛了异常，它已有的状态不变，计一次内部错误，这个进程内不再重试这一次查看（重启后重新来过）；`apply_enables`、`maybe_notice`、写状态文件这些例行步骤的失败也不得让服务退出（计数，每步只在 stderr 说一次）。**写不进 journal 是唯一不被吸收的失败**：`journal.append` 的写或同步一旦失败，该 journal 此后不再接受任何记录（`JournalWriteError`），引擎不再处理新请求（不读、不发），桥以**退出码 7** 退出并说明原因；journal 里已落盘的部分是一条有效的链（带或不带最后那一行；写了一半的最后一行由启动时对撕裂行的处理切掉），下次启动照常重放，已发出的写不会再发。写不出状态文件在**启动时**就拒绝启动（退出码 2），而不是之后才发现。

**同步能力与失败策略**（IR02，独立修复副本）：平台提供 `F_FULLFSYNC` 时，使用它；任何调用错误（包括 EIO、ENOSPC、EINVAL、ENOTSUP／EOPNOTSUPP）都向上传递，不降级为普通 `fsync`。只有平台没有该接口时才使用 `fsync`，其错误同样向上传递。journal 追加时的同步错误按上段处理：进入失败状态，不再处理新请求，退出码 7。兼容性取舍：提供完整同步接口但文件系统不支持它的环境会拒绝继续运行，需要支持相应同步的存储；不能悄悄降低持久性保证。启动打开／修复 journal 阶段的错误不属于“运行中退出码 7”的承诺范围。本项不等于已验证断电或硬件实际落盘。

**终态发布恢复**（IR03，独立修复副本）：已有 durable `terminal` 记录时，trace 写入或同步失败只增加内部错误计数，并把动作留在待发布集合；不得因此隔离原请求、改写终态或再次调用 IA2。相同请求重放、启动恢复及每 2 秒的例行步骤均可补发 journal 中同一个事件；每轮例行步骤最多处理 100 个动作，失败项轮转至队尾。启动通过整个 journal 的终态定位索引找回尚未发布的结果，包括旧版本已误隔离的请求及超过热索引期限的动作；旧隔离记录保留。状态文件以 `pending_trace_results` 显示待发布数量。journal 已失败时停止补发，沿用退出码 7。

trace 使用无缓冲二进制写入，完成全部字节并同步之后才记为已发布；同一进程重试先截去未确认的写入后缀。启动保留撕裂尾行的独立副本、截去尾行，并同步已有完整行后才据其事件 ID 去重；不是桥能写出的完整行（含非 ASCII 字节、不是 JSON 对象、事件 ID 不是字符串、嵌套过深无法解析）一律跳过，不据其去重、不取其锚，启动时在 stderr 报出行号（其中若有本该去重的事件，可能再发一次）；锚只认非负整数的 `journal_seq` 加字符串的 `journal_hash`；完整行已落盘但之前同步报错时不追加重复行。补发可更新 journal 锚，事件 ID、状态及业务证据保持原值。消费者应按事件 ID 去重：此规则不承诺消费者从未看到过未同步的旧行，也不实现其它种类事件的持久重试队列。

### 8.3 `/audit` 对账

- IA2 的 `/audit` 是**内存环、容量 256、重启即清**，只能佐证，不能当唯一记录。
- 既有只读采集器每次轮询（约 1 s）归档一次 `/audit`（`collector.py` 的 `ring(... 'write_audit' ...)`），所以只要桥 + 其他写入者在一个轮询间隔内远低于 256 条就不丢；G6 把全桥限制在 ≤ 60 次/分正是为此。
- 对账程序**必须**区分三类：journal 有且审计有（一致）；journal 有而审计无（环已回绕、实例重启，或写入在客户端断开后才执行而没留条目（§11.5）→ 记"不可核对"，**不**据此推断"没写过"）；审计有而 journal 无（**不明写入**：可能是别的写入者，也可能是桥之外的路径 → 告警并保留）。
- **实现（v0.10，WP6）**：`tools/reconcile_report.py`，只读文件、不导入桥的代码。输入是 journal、约每秒一次的 `/audit` 归档（每行带读取前后两次 `runtime_id`；同一实例两次读取之间没有重叠，或某实例第一次读到的环已满 256 条，记为回绕缺口）、设备自己的写日志，以及可选的生命周期与代理日志。每个 intent 归入一类：`consistent`（设备没收到应用的值时，须有理由：IA2 说未送达、设备已持有该值、设备收到请求未应答、一个输出周期（2 s）内被替换，或设备日志证明从写入到被替换（或完整日志结束）之间没有可用链路）；`executed_unaudited`（归档无缺口地覆盖了它、设备收到了值、桥没收到 HTTP 答复）；`not_sent`；`no_effect_no_entry`；`uncheckable_restart`（有后续实例为证）；`uncheckable_turnover`；`uncheckable_archive`；`uncheckable_device_log`；`unexplained`。**有 HTTP 答复却没有条目算差异**：runtime 的 `/write` 处理函数对每个进到它的请求都记审计（只有请求体解析失败的 4xx 例外，桥不发那种请求）。另列不明写入，以及没有请求值、初值、程序自赋值或安全态能解释的设备写入。设备日志与生命周期只在实验室有：生产环境里 `executed_unaudited` 与 `no_effect_no_entry` 分不开，只能记"不可核对"。
- **替换（v0.11，审核 FIX01）**：只有**确认已应用**的后续写入才能解释"设备没收到前一个值"：它在 `/audit` 恰有一条与其 intent 相符的条目、带 `applied`、与桥收到的 HTTP 答复不矛盾（没有答复也可以）、与前一个值在**同一 runtime 实例**、在环里**排在其后**（审计时间只到秒，先后取环内顺序），且应用的是**另一个值**（取 `applied`，不取 `requested`；同值再写不解除送达义务）。被治理拒绝、没发出、超时且无条目的请求都不算。实例结束（有后续实例为证）另作一种结束。
- **设备日志覆盖（v0.11，审核 FIX02）**：设备日志必须能完整解析、以设备的 `listen` 开头；它覆盖从那里到最后一条记录（完整日志以 `stopped` 结尾），`stopped` 之后又有记录、或 `seq` 不连续，都是覆盖缺口。"没有应答 ⇒ 没有可用链路"只在日志覆盖整段、且有记录在运行的证明时成立：显式的链路断开 `mode`（silent / close / refuse）区间，或连续的 `seq`；段的终点必须有限且有依据（确认应用的替换、实例结束、完整日志的结尾）。2026-10-29 之前的日志没有 `seq`：只有显式 `mode` 区间能证明断链。覆盖不到、需要它的判断记 `uncheckable_device_log`。设备映射里的每个设备都必须有日志；日志为空、缺开头、记录损坏、`seq` 时有时无，以及没有任何可用轮询的 `/audit` 归档，都是不可用的输入。
- **结论与退出码（v0.11）**：有不明写入、`unexplained`、无法解释的设备写入或 journal 链断 → `DISCREPANCY`（1）；否则有 `uncheckable_archive` 或 `uncheckable_device_log` → `INCOMPLETE`（2，报告照写）；否则 `CLEAN`（0）。输入不可用时退出码 2、不写报告。`uncheckable_restart` 与 `uncheckable_turnover` 是有证据的不可核对，不影响 `CLEAN`。

### 8.4 启动自检与运维可见性（v0.7）

桥启动时依次检查，任何一项不过都拒绝启动并说明原因（退出码）：

| 码 | 检查 |
|---|---|
| 2 | 注册表不是严格 JSON / 哈希不符 / 有非支持类型 / 有 `enabled` 且 `requires_approval` 的项；**密钥文件对组或其他人可读**；journal 链断了；**状态文件写不出**（P1） |
| 3 | 工程目录里的 `northbound.toml` 不可读、不是合法 TOML、`mqtt` 不是表，或显式 `mqtt.allow_write` 不是布尔 false；缺文件、缺表或省略字段沿用运行时 false 默认值（D6） |
| 4 | 预检：能连上的 runtime 没有 `runtime_id`，或某个目标的反馈变量不在快照里 / 没有 `input.stale`。**不支持没有 `runtime_id` 的旧构建**（连不上则推迟，每个请求的 G8 仍会核） |
| 5 | 注册表范围超出工程 `project.toml` 的 `[governance]`，或变量不在 allowlist（`--governance-check strict`，默认；`warn` 只记录，`off` 跳过） |
| 6 | journal 比 trace 里最高的锚短，或同一序号的哈希不一致（journal 被截尾 / 重写） |
| 7 | **运行中** journal 写不进或同步失败（P1，F07）：桥停下，不再发送任何东西；下次启动重放磁盘上的内容 |

每条 trace 事件带写入时的 journal 头（`journal_seq`、`journal_hash`），这是 journal **之外**的锚。journal 与 trace 在磁盘上是纯 ASCII，落盘同步在 macOS 上用 `F_FULLFSYNC`。journal 只给最近 24 小时内的动作建索引（整个文件的哈希链照样验证）。**被桥禁用的目标**（404）由运维写 `<journal>.enable` 文件重新启用（`{"target_key", "reason"}` 一行一条，经 journal 记成 `enable`）。**无法归属到动作的请求**（验签失败、非严格 JSON、超过 64 KiB 的行、内部错误）没有逐动作的回答，运维靠 `<journal>.status.json` 的计数和每分钟至多一条的 `bridge_notice` 事件（只有计数）看到它们。

## 9 v0 明确不做

`pulse_ms` 脉冲写（复位所有权、被其他写取代、设备断开时零仍留在 VM 的语义，留待后续版本）· force/unforce（ADR-0002：从不导出）· pause/resume/step/stop · `inject-scan-stall` · 报警确认（操作员动作）· 部署/配置 · 多变量原子写与跨设备顺序 · LREAL（HTTP 的 `value` 是 i32，表达不了非整数 f64）· 非本机访问与鉴权设计（ADR-0002：必须先有鉴权设计才能有非 localhost 绑定）· MQTT 北向写通道（无逐请求响应：Agent 学不到 clamp/拒绝/未送达；**应当**保持 `allow_write = false`，见 C10 和 D6）· 人工批准流程（D3）· 任何接真实设备的执行（阶段 3，条件见 README）。

## 10 决定记录（D1–D14）

2026-10-27 用户答复：D2 采用 HMAC 签名；随后「D1-8 采用最左侧的推荐项」；再随后「D9 用 JSON、D10 按精确字节签名」。2026-10-28 用户对「trace 事件（和样本）要签名吗？」答「签名」（D11）；随后点选「(e) 专用采样器」（D12），该按钮同时带着 D13=HMAC、D14=删不签名路径、先复核再实现。下表「决定」列就是各项原先排在最前的选项；「未选」列留着，将来复议时知道换的是什么。

| # | 问题 | 决定 | 未选 | 对契约 / 用例的影响 |
|---|---|---|---|---|
| D1 | `undelivered` 之后的迟到命令 | (a) **不撤回**：只标 `undelivered` 并告警 | (b) 桥自动写回动作前的值 | M3 不含撤回。C3 只跑 (a)，记录迟到时长与迟到命令的实际值，留作复议依据（若显示迟到命令可在 deadline 之后很久才执行且值仍有危险，则复议）。桥参考实现**不写**撤回路径 |
| D2 | 请求真实性 | (a) **HMAC 签名**（§3.1） | (b) 只信 Tier0 命名空间；(c) 控制入口不经 Tier0 存储，直接 SSH stdio 给桥 | 1a 用本机 0600 密钥文件；生产的密钥放置与轮换随 D5 在阶段 2 前定。签名对象的字节级细节见 `DESIGN-1A.md` §3.3 |
| D3 | 是否要人工批准层 | v0 **没有**；注册表预留 `requires_approval` | 现在就做批准层 | 预留字段在 v0 必须 **fail closed**：任何 `enabled` 且 `requires_approval=true` 的注册项使桥拒绝启动（新用例 F5）。阶段 3 的第一个真实目标仍要人工批准（README） |
| D4 | 读侧合同升 v2 | **升**：新增 rejected / modified / undelivered 与 `claim` / `not_claimed`；v1 消费者按「非成功」处理 | 不升，把新状态折叠进 `unknown` | 读侧 `derive_action` 要**重写**而不是「扩充」：v1 从一条 `verified` 终态事件出发再查引用，v2 必须从 `accepted` 加观测出发，忽略任何 `verified` 声明（`DESIGN-1A.md` §4） |
| D5 | 桥的部署形态 | grey0 上**独立用户服务**；独立 Tier0 密钥（仍是实例级权限）；CPU 亲和避开 {5,6,7,13,14,15} | 与只读采集器同进程 / 同服务 | 阶段 2 才用；1a 在 Mac 上不涉及 |
| D6 | 现役工程的 `northbound.toml` 是否启用 `allow_write` | 部署前只读核对；桥启动及逐请求解析 TOML，**拒绝**显式 `mqtt.allow_write` 不为布尔 false 或无法解析的配置 | 不自检 | `1c42df7` 的 runtime 源码里 `/health`、`/status` 不暴露北向状态（核对过 `main.rs`），所以自检只能读部署目录里的 `northbound.toml`，见 `DESIGN-1A.md` §3.6。缺文件、表或字段按运行时缺省 false 处理；转义键和字符串内 `#` 由完整解析器处理，不作文本匹配。C10 改为这项自检；新增原因码 `northbound_write_enabled` |
| D7 | 标签里嵌 action 前 8 位 | **嵌**：`tier0.<agent>.<id8>` | 固定标签 | 对账用；代价是 IDE 横幅每个不同标签闪一次（文档已说明） |
| D8 | 崩溃恢复后，对「有意图、`/audit` 里查无条目」的动作 | (a) **不补发**，保持 `unknown` | (b) deadline 内、实例未变时补发一次 | v0 不实现补发路径；R1 只跑 (a) |
| D9 | 注册表文件格式 | **JSON**（零依赖；Mac 的 Python 3.9.6 没有 `tomllib`） | TOML | §3.2 已改；解析器拒绝重复键与未知字段 |
| D10 | 签名对象 | **`payload` 的精确字节**（信封，先验签后解析） | 规范化 JSON | §3.1 已改；F2 增加「签名正确但 payload 非法」变体 |
| D11 | trace 事件与反馈样本要不要签名 | **签**：桥给每条 trace 事件签名，采样器给每条样本签名，读侧（验证器）只认签名有效的；域前缀 `ia2.event/v0\n`、`ia2.sample/v0\n`，与请求的 `ia2.control/v0\n` 互不通用；三把密钥互相独立；**只签事件不够**（只伪造样本也能凑出 `verified`），所以样本必须同样处理 | 不签，靠 Tier0 命名空间 | **只有设计（`DESIGN-D11-signed-events.md`），代码未实现。** 实现时：§3 之外新增事件 / 样本信封、§7 前加验证门（V1–V6）、验证器拒绝在没有密钥时工作；新增用例 F6–F8（`TEST-CASES.md` 暂未收入）。N-1 在实现并复核前仍然开着 |
| D12 | 样本由谁签 | **(e) 专用的已签名采样器**：独立只读进程，只 `GET /status`（需要时加 `/health`），自己的密钥与主题 | (a) 改现有只读采集器；(b) 签名旁路；(c) 读侧直读 IA2；(d) 桥当第二观察者 | 不动哈希钉住的采集器包；采样频率对得上验证窗口。上 grey0 前要测多出的 `/status` 读对 #65 的影响（**没测**）。1a 里就是给 `lab/sampler.py` 加签名 |
| D13 | 对称还是非对称 | **HMAC** | Ed25519（读侧只持公钥，被攻破时不能伪造，但要引入依赖） | 「读侧被攻破即可伪造事件 / 样本」写进 §7 的限制；将来升级是改信封的 `v` |
| D14 | 是否保留「不签名」的路径 | **删**：trace 永远是信封，验证器没有宽松模式，桥没有关签名的开关 | 保留实验室开关 | 桥缺事件密钥时启动失败；验证器缺密钥时拒绝工作 |

D11–D14 都**只有设计**（`DESIGN-D11-signed-events.md`），实现等独立复核回来后与复核结果一并做。

## 11 本契约依赖的 IA2 / Tier0 事实

出处均为 `FREEZONEX/ia2` 的 `origin/main` = `1c42df7`（2026-10-27 核实）；行号是该提交上的行号。把握：**读**＝读码/读文档，没运行；**测**＝在台架或假设备上测过。

| 事实 | 出处 | 把握 |
|---|---|---|
| `/write` body 为位编码 i32；响应键 `ok,name,value,undelivered_device`；403/404/409/500 | `crates/runtime/src/main.rs` `write_response` L1610、`rt_write` L1621、`write_err` L1586；`docs/api.md` L530 | 读 |
| `undelivered_device` 只看设备传输健康，**不看锁存**；注释"report its own transport only" | `crates/ironplc-bridge/src/runtime.rs` `mapped_write_outcome` L1930 | 读 |
| 锁存时输出阶段被跳过，所以锁存后的写会 200 且永不下发（F2） | 审计 `39a04dd`：假设备测试复现；`1c42df7` 与 #96/#98 的 diff 里**没有**对 `mapped_write_outcome`/`undelivered_device` 的改动（diff 检查，非运行） | 测（旧基线）/读（新） |
| 设备断链：写仍进 VM，链路恢复后"当前值"被下发（迟到命令） | `docs/api.md` L530（"a Stop that flushes when the link returns…"）；`docs/api.md` L339 | 读 |
| 治理：`write_mode = open|allowlist`；规则 `variable/min/max`；REAL 按 f32 位型，其余走整数道；NaN 与空区间拒绝 | `crates/ironplc-bridge/src/runtime.rs` `enforce_write_governance` L1973；`crates/project/src/types.rs` | 读 |
| `/audit`：256 容量、内存、重启清空；`origin` 自称；有拒绝条目；结果文本含 "not delivered to device … (link down)" | `main.rs` `AUDIT_CAP` L1446、`write_audit_outcome` L1543；`docs/api.md` L534 | 读 |
| `X-IA2-Origin` 清洗：IDE server 保留 `[A-Za-z0-9._-]`、≤64；edge runtime `origin_of` 取非空原值 | `docs/api.md` L407–L415；`main.rs` `origin_of` L1565 | 读 |
| `/health` 含 `fieldbus_healthy`、`devices`、`watchdog_tripped`、`latch_reason`、`fault`、`snapshot_tap`；`/status` 含 `mode`、`forces`、`device_health`、`scan_overruns`、`runtime_id` | `docs/api.md` L520–L521；`main.rs` L983 附近 | 读 |
| 快照里 Input 映射变量带 `input:{device,channel,stale}`；`stale` 时值是"最后值，不是新鲜测量" | `docs/api.md` L362–L365 | 读；**现役 HMI 试运行版比 main 落后约 30 个 PR，是否输出 `input.stale` 未核对** |
| 北向 MQTT `<prefix>/write` 仅 `allow_write = true` 时订阅，**无逐请求响应** | `crates/runtime/src/northbound.rs` 文件头 L15–L18；`crates/project/src/types.rs` `NorthboundConfig` | 读 |
| runtime 无鉴权，只绑 127.0.0.1，远程靠 SSH；非 localhost 绑定前须先有鉴权设计 | `docs/api.md` L8；`docs/adr/0002-…md` 决策 3 | 读 |
| PR #96（头 `4aa6dd4`，未合并）新增 `failsafe_status {pending, acknowledged, unconfirmed}` 到 `/health` 与 `/status`；#98 叠其上（头 `5ca976e`） | `gh pr diff 96`（2026-10-27） | 读 PR diff，字段名以合并版为准 |
| 既有采集器把快照变量逐字复制（`{**v, decoded_value}`）并附 `device_health/mode/forces/watchdog_tripped/snapshot_tap`，且每轮归档 `/audit` | `ia2-tier0-readonly-20261023/payload/app/ia2_ro/collector.py` L~90–L110 | 读 |
| Tier0 无逐主题密钥 ACL；Action 去重仅进程内；结果判定由验收脚本实现、不是 Tier0 原生 | `tier0-acceptance-20261022/RESULT.md` | 读文档 |
| 读侧 v1：状态四值；证明须在请求至截止之间，质量 Good，关联同会话/设备/动作 | `ia2-agent-observation-20261022/CONTRACT.md`；`tier0-acceptance-20261022/acceptance.py` `derive_action` | 读 |

### 11.1 WP0 实测补充（2026-10-27）

在 `1c42df7`（Mac 构建，二进制 `cf64e6a2…`）+ 回环假 Modbus 从站上**测过**的条目。证据在 `tier0-ia2-control-1a-20261027/RESULT-WP0.md` 与 `wp0/run-*/golden/`。上面 §11 表的「读」栏不逐条回改，以本节为准。单次观测，没有重复，没有 Linux 对照。

| 事实 | 把握 |
|---|---|
| `/status` 有 `runtime_id`（`<PID>-<启动纳秒>`）与全 SHA 的 `build_commit`；`last_snapshot` = `{device_health, scan_count, timestamp_us, vars}`；变量 `{name, type_name, value(字符串), bits}`；**只有 Input 映射的变量**带 `input:{channel, device, stale}` | 测 |
| `/write`：REAL 位型逐位往返；clamp 时 `value` 是 clamp 后的位型；`undelivered_device` 键在每个 200 响应里都在；403 / 404 / 409 的体是 **text/plain** | 测 |
| `/write` 的变量名**逐字精确匹配、区分大小写**：裸名或 `<实例名>.<变量>` 可以；程序名前缀、大写、混合大小写都 404（治理规则按文档不区分大小写，但这里的名字解析区分） | 测（单实例工程） |
| IA2 **不校验 BOOL**：写 2 → 200，设备收到 ON；**`+inf` 被 clamp 成 max**，只有 NaN 是 403 | 测 |
| 输出**只在值变化时下发**；被程序每周期赋值的输出收不到监视写入，响应仍是 200 / `undelivered_device:null` / 审计 `ok`，设备从未收到该值 | 测 |
| 断链写入：200 + `undelivered_device:"<设备>"` + 审计 `ok; not delivered to device '<设备>' (link down)`；恢复后值迟到下发（约 1.8 s，单次）；**迟到下发没有新的 `/audit` 条目** | 测 |
| 线上功能码：BOOL → FC05；REAL → FC16 两寄存器 `hi_lo`；INT(i16) → FC06；DINT → FC16；读线圈 FC01、寄存器 FC03 | 测（假从站日志） |
| 有符号整数的 `bits` 是符号扩展成 u64 的（DINT −5 → `18446744073709551611`） | 测 |
| `close` 模式断链被识别约 0.32 s（单次）；`silent` 模式未测 | 测 / 未测 |
| `/status` 与 `/health` 都带 `scan_count` `watchdog_tripped` `latch_reason` `fault` `snapshot_tap`；无 `failsafe_status` | 测 |
| 只读采集器：`Source` 固定目标 `127.0.0.1:13001`，`process_identity` 读 `/proc`；在 macOS 上不能原样运行 | 测（import 与两处调用） |

### 11.2 切片实测补充（2026-10-28）

在桥参考实现 + 录制代理 + 独立验证器上，各 3 次（`tier0-ia2-control-1a-20261027/runs/`，`RESULT-SLICE.md`）。

| 事实 | 把握 |
|---|---|
| IA2 发布的 `scan_count`（`/status` 顶层、`last_snapshot`、`/health`）约每 109 ms（中位；66–131 ms）才跳一步，一步 10–11 次扫描；任务周期 10 ms。这是快照任务的节拍，不是 `scan_period_ms` | 测（`probe-scan-count-cadence.txt`，28 步） |
| **F2 在 `1c42df7` 复现**：向已锁存（`watchdog_tripped`，`latch_reason` = `scan watchdog: 5 consecutive overruns on main_inst`）的 runtime 写：200、`undelivered_device:null`、响应 `value` 等于请求、`/audit` 记 `ok`、VM 变量变成请求值，**设备在锁存后没收到任何写入** | 测（C1(b)，3/3） |
| 正常桥在 G8 拒绝锁存状态下的写，没有任何 `POST` 到 IA2；关掉 G8 的诊断桥写进去后，发送后检查把它记成 `unknown latched_after_write` | 测（C1，3/3） |
| 13 种非法请求（未登记键、IA2 变量名、实例限定名、8 种非 `ia2_set` 命令、`pulse_ms`、`simulation` 不符）全部在桥内被拒，录制代理的线上日志为空，`/audit` 为空 | 测（B1，3/3） |
| 治理 403：桥只发一次、不重试；`/audit` 记一条没有 `applied` 的拒绝；变量与设备不变 | 测（B3，3/3） |

### 11.3 P0 实测补充（2026-10-28）

24 个 P0 用例的 3 遍运行（`tier0-ia2-control-1a-20261027/runs/`、`RESULT-P0.md`）。回环假从站，单个构建，少量观测。

| 事实 | 把握 |
|---|---|
| 向 IA2 写 BOOL=2 返回 200、设备收到 ON；`+inf` 被 clamp 成 max；只有 NaN 是 403；`-0.0` 被放行（桥自己拒） | 测（WP0、A2、B2） |
| 向被程序每周期赋值的输出写：200、`undelivered_device:null`、审计 `ok`；VM 回到程序值；设备从未收到；反馈不动。输出只在值变化时下发 | 测（C6，3/3） |
| 静默的从站：运行时的健康位约 **1.2 s**（每遍中位 1.198 / 1.199 / 1.21 s，24 次在 1.17–1.223 s）后才翻。窗口内桥放行的写（每遍 15–16 个中的全部）都 `undelivered_device:null`，没有一个能被验证 | 测（C4，3 遍） |
| 断链写入的迟到下发：链路恢复后 **2.67–2.78 s** 下发请求值（`close`、`refuse` 一致；deadline 之后才恢复的节奏下，约在请求后 7 s、deadline 后 3 s）；WP0 里断开约 3 s 的那次是 1.8 s。`/audit` 无新条目 | 测（C3，3 遍） |
| 被杀后重启的 runtime 在启动时对设备输出写 0；`/audit` 清空；新 `runtime_id` | 测（C8，3/3） |
| 桥超时（3 s）后关闭连接，一个排队在被 `SIGSTOP` 的 runtime 前面、尚未被读走的请求，在 `SIGCONT` 之后**没有**被执行（`/audit` 为空）。挂起发生在请求被读走之后的时序没测（2026-10-29 已测，见 §11.5） | 测（R3，3/3） |
| 向已锁存的 runtime 写：200、`undelivered_device:null`、VM 变量变成请求值，设备无写入（F2） | 测（C1，6/6） |
| `http.client` 的 `read(n)` 对短于 `Content-Length` 的响应体不报错 | 测（B11，一个客户端缺陷） |

### 11.4 修复 W1–W19 之后的实测（2026-10-28）

`tier0-ia2-control-1a-20261027/RESULT-FIXES.md`。75 次运行（25 个用例各 3 遍）、94 个单元测试。

| 事实 | 把握 |
|---|---|
| 生产打包的桥（无钩子加载代码）端到端 `accepted` → `verified` | 测（`PROD`，3/3） |
| 探测：`runtime_id` 缺失、反馈变量缺失 / 无 `input.stale` 时桥以退出码 4 拒绝启动 | 测（单元测试里起假 runtime） |
| 验证在扫描时钟里做，采样器墙钟再偏也不影响结论 | 测（向量） |

### 11.5 R3 时序覆盖（2026-10-29）

运行时 `047e7c1` 构建（源码同 `main` `e6a38aa`，SHA-256 `52160ab8…`），回环假从站。桥的 POST 超时为默认 3 s。真实情况取自假设备写日志、VM `/status`、`/audit`；挂起时序由 runtime 一端套接字的 `Recv-Q` 证明。

| 事实 | 把握 |
|---|---|
| 请求还没读走时 runtime 被挂起（`Recv-Q` = 请求长度）：桥关闭连接、`SIGCONT` 之后，请求**没有**执行（VM 不变、设备无写入、`/audit` 无条目） | 测（R3，3/3） |
| runtime 已读完请求（`Recv-Q` = 0），写入排在扫描循环里还没执行（注入一次 1 s 扫描停顿，停顿中 `SIGSTOP`）：桥超时、关闭连接之后，写入**照样执行**（VM 为请求值、设备收到 FC16），但 **`/audit` 没有条目** | 测（R3T1，3/3） |
| 同一时序，只让 runtime 一侧的连接保持打开：写入执行，`/audit` **有**条目。缺条目由客户端断开引起 | 测（R3T1K，3/3） |
| 同一时序，改由桥被 `SIGKILL` 触发：写入执行，`/audit` 无条目；重启后的桥结论为 `unknown / bridge_restarted` | 测（R3T1X，3/3） |
| runtime 已执行并应答、响应没送到桥（代理扣住响应并同时挂起 runtime）：`/audit` 有条目，桥第一次对账即 `found` | 测（R3T2，3/3） |
| 原因（读码）：`crates/runtime/src/main.rs` `rt_write` 在 `write_with_pulse(...).await` **之后**才 `record_audit_http`；`ProgramHandle::send_write` 先把命令放进通道再等应答；hyper 1.9 默认 `h1_half_close = false`，忙时读到 EOF 就结束连接并丢弃处理函数。`rt_force`、`rt_unforce` 同样写法（未运行）。与 `rt_audit` 的文档（"Every HTTP /write·/force·/unforce … lands here"）不符，属 IA2 运行时缺陷，修复不在本 PR | 读；由 R3T1K 对照证实 |
| 修复后的 runtime（分支 `fix/control-write-detached` 提交 `1cc16c0`，SHA-256 `c18b0aaa…`）上同样的时序：R3T1、R3T1X 写入执行且 `/audit` **有**条目；R3 不执行；R3T1K、R3T2 执行且有条目。v0.11 起 R3T1、R3T1X 的 EXPECT 按此（`main` 修复前的 runtime 上为 FINDING） | 测（2026-10-29，整套里各 3/3；Python 3.14.8 上也通过） |
| 同一修复分支的第一个提交（`48dec8a`）把脉冲的复位截止时刻取在调用时：排在 300 ms 扫描停顿之后的脉冲写入，在回环假设备上 800 ms 只保持 530 ms、100 ms 只保持 9.5 ms。`1cc16c0` 改为写入落地后起算：801–810 ms 与 109–110 ms（3 遍；`main` 809.5 / 110.4 ms） | 测（`lab/check_pulse_hold.py`，审核 FIX03） |

### 11.6 P1 与浸泡实测（2026-10-29）

同一 runtime 构建（`52160ab8…`），回环。

| 事实 | 把握 |
|---|---|
| 直连 runtime 时 `origin` 原样记录非空的 `X-IA2-Origin`：`.` `-` `_`、`:`、空格、70 个字符都保留；空串与不带头都记 `anonymous` | 测（A3） |
| `/audit` 写满 256 条后丢最早的：经桥写 300 次后只剩最新 256 条 | 测（A4） |
| 锁存时 IA2 仍对 `/write` 答 200（C1(b) 之外再一例：G8 通过之后才跳闸） | 测（C2） |
| runtime 暂停时直写答 200、VM 收下、设备不写；目标被强制时直写答 200 且回显请求值，VM 仍是强制值、设备不写。两种情况桥都在发送前拒绝（暂停时扫描计数不动，G8 先拒；强制时 G9） | 测（C5） |
| coupler 不健康时 `/health.status` 仍为 `ok` | 测（C7） |
| 锁存时 `/health` 与 `/status` 都带非空 `failsafe_status`；正常时为 `null` | 测（C1，`main` 即 #98 的合并） |
| 桥一次读一行请求；读到时距 `requested_at_ms` 超过 2 s 的按 G4 `expired`。所以一秒内涌入的大批请求，排在后面的会过期，而不是被限速 | 测（R5 第一次运行：50 个里 12 个 `expired`） |
| `POST /stop` 之后 runtime 进程退出；之后的写入被 G8 以 `runtime_unreachable` 拒绝，409 路径从桥走不到 | 测（B5） |
| 看门狗锁存与停机都把全部输出（含程序自赋值的输出）写成 0 | 测（C1 的设备日志；浸泡） |
| 设备关闭连接约 0.2 s 后、IA2 尚未察觉时，对写入仍答 200、`undelivered_device: null`、审计 `ok`；该值随后被新写入替换，链路恢复时只下发新值，旧值从未到达设备（C4 静默窗口之外的同类情况） | 测（浸泡，1 次） |
| 2 小时混合浸泡（40 次故障；请求循环 4496 次，请求文件 4848 行，签名有效的 4486 行里有 487 行是重复提交，新动作 3999 个）：2179 个 intent 中 2166 个三方一致；6 个客户端断开后执行而无条目（R3T1 精确点）、1 个请求未被读走（R3 时序）、5 个没发出（R1）、1 个因实例重启不可核对；不明写入 0 | 测（`../RESULT-1A.md` §3） |
| 修好审计缺口的 runtime（单独分支的第一个提交）上 60 分钟同样的浸泡（请求循环 2252 次，文件 2440 行，签名有效 2289 行，新动作 2039 个）：1128 个 intent 中 1125 个一致、3 个没发出，R3T1 精确点 2 次都有条目 | 测（同上） |

## 12 变更记录

| 版本 | 日期 | 内容 |
|---|---|---|
| v0 草案 | 2026-10-27 | 首版。基于 `1c42df7` 读码；无任何运行验证。 |
| v0.1 | 2026-10-27 | 记录 D1–D8 的决定（§10）；D3 的预留字段改为 fail closed；D6 的自检改为读 `northbound.toml`（运行中的 runtime 不暴露北向状态），新增原因码 `northbound_write_enabled`；M3 / §8.2 删去「撤回」「补发」的待定措辞。其余条文与 EXPECT 未变。 |
| v0.2 | 2026-10-27 | 用户定 D9（注册表用 JSON）、D10（对 payload 精确字节签名）：§3.1 改为信封 + payload，§3.2 改为 JSON 字段说明，§10 增 D9/D10。其余条文与 EXPECT 未变。 |
| v0.3 | 2026-10-27 | WP0 实测后修订：§3.2 示例里的 `ia2_variable` 改为裸名并说明名字规则；§6 M3 注明迟到下发不进 `/audit`；新增 §11.1。EXPECT 的修订见 `TEST-CASES.md` 变更记录。 |
| v0.4 | 2026-10-28 | 最小切片（A1、B1、B3、C1）运行后修订：G8 改为在 `advance_wait_ms` 内等 `scan_count` 前进（固定间隔读两次会误拒）；G5 前移到 G1 之后；G1 明确未知字段与非 `ia2_set` 命令为 `bad_request`；单飞的「未终结」定义；§7 证据窗口从 `sent_at_ms` 算起并以 `settle_ms` 封顶；新增 §11.2。 |
| v0.5 | 2026-10-28 | 24 个 P0 运行后修订：G1 对 payload 非严格 JSON / 验签失败 / 信封畸形只记 journal；G3 `-0.0`、±∞ 为 `bad_value`，非支持类型注册表加载即拒；G5 冲突以 `conflict_notice` 回报且不改原状态；M10 增加对账记录；M11 由读侧推导；§8.2 崩溃恢复表按实现改写；§7 第 7 条（`verified` 不证明是这次写入的原因）；原因码 `bridge_restarted`；新增 §11.3。 |
| v0.6 | 2026-10-28 | 复核前的畸形输入探针查出 5 个让桥进程退出的输入（`target = ±10**400`；孤立代理字符出现在 `target_key` / `session_id` / `device_id`），已修并整套重跑：超大整数 → `out_of_range`；journal / trace 纯 ASCII；§8.2 增加「未预期的异常」一段（`internal_error`、按行哈希隔离、在途动作收尾）；已记过的未验签 / 非严格 JSON 行只记一次。 |
| v0.7 | 2026-10-28 | 修复 W1–W19：验证只信桥的事件并在扫描时钟里排序（§7 第 0、3 条，事件新带 `post_scan_us`、`scan_anchor_ms`、`registry_sha256`）；G4 按到达时刻；G5 后加标签冲突；G6 单飞窗口 `single_flight_ms`；`requested` 由读侧派生；`post_check_ms` 换成读超时；M10 对账看两次；§8.2 恢复收尾带恢复时刻的标记；新增 §8.4 启动自检（退出码 2–6）、trace 锚、enable 文件、运维计数；待决定 D11（trace 事件签名）；§11.4。 |
| v0.8 | 2026-10-28 | 用户定 D11（trace 事件与样本都要签名）：§10 增 D11；同日用户点选「(e) 专用采样器」，一并记入 D12=(e)、D13=HMAC、D14=删不签名路径、先复核再实现；**只有设计文档 `DESIGN-D11-signed-events.md`，没有改任何代码或用例**，条文（§3、§7、§8.4）暂未按 D11 改写，实现时再改。 |
| P1-b1（独立副本） | 2026-10-28 | **这是独立修复副本里的记录，不是原项目的契约版本。** P1 第 1 批（F05 + F06）：§7 增第 8 条，规定读侧比较是按注册表类型的精确绝对差（十进制往返表示；BOOL 精确相等、容差不参与；非有限 / 类型不符的反馈不是观测；无效的请求或容差直接 `unknown`）。读侧验证器需要注册表项带 `type`。其余条文未动。 |
| P1-b2（独立副本） | 2026-10-28 | **独立修复副本里的记录。** P1 第 2 批（F08 + F09）：§8.2 增"落盘什么、重启信什么"——`response` 记录带随完整响应得出的版本化 `facts`（与诊断副本分开，同一条记录写入），`post_check` 记录带版本与归属并在恢复时用与现场同一条规则；恢复时的读数只能增加限制；P0 记录的兼容规则。新增崩溃点 `after_post_check`（仅实验室构建）。 |
| P1-b3（独立副本） | 2026-10-28 | **独立修复副本里的记录。** P1 第 3 批（F10）：§5 增"每次 HTTP 操作只有一个总预算"——单调时钟、连接/发送/响应头/正文/分块元数据共用、每次底层读写只拿剩余时间、超时不重试；响应头、分块元数据、正文各有字节上限且在到达时就生效。客户端不再使用 `http.client` 传输（`bridge/ia2client.py` 自带严格的小型读取器；请求头多了 `Connection: close`）。 |
| P1-b4（独立副本） | 2026-10-28 | **独立修复副本里的记录。** P1 第 4 批（F07）：§8.2 增"异常收尾本身不能再击倒桥"——收尾按动作隔离、失败时只凭 journal 结束在 `unknown`（不读网络、不发送）、启动恢复同样适用、周期对账与例行步骤的失败不退出服务；journal 写不进是唯一不被吸收的失败（`JournalWriteError`，引擎不再处理新请求），§8.4 增退出码 7，状态文件写不出在启动时拒绝（退出码 2）。 |
| P1-D01（独立副本） | 2026-10-28 | **只改设计文档，条文与代码不动。** `DESIGN-D11-signed-events.md` §1、§7 订正"伪造 `verified` 需要同时偷到桥的事件密钥和采样器的密钥"：事件与样本分别认证来源、依赖各自签名者正确报告事实，任一事实来源出错再碰上另一端真实且匹配的记录就可能形成错误 `verified`；独立密钥不是双人授权；§7 加信任矩阵并写明验证器识别不了"签名有效但事实错误"。**D11 仍然没有实现。** |
| IR01（独立修复副本） | 2026-10-28 | §5 明确响应边界的唯一性；拒绝重复／冲突的长度和编码头、两者并存及解析器报告的畸形头，避免协议不确定的响应成为成功证据。 |
| IR02（独立修复副本） | 2026-10-28 | §8.2 明确同步错误不得降级：接口存在时任何完整同步失败均传播；仅平台没有完整同步接口时用普通 fsync。IR03、D11、其它 P2 未修。 |
| AR01（独立修复副本） | 2026-10-28 | §5 统一 CRLF／LF 头边界，拒绝完整头块中的裸 CR、解析残留 payload 与 Unix mailbox 分隔行，防止字段被静默丢弃。IR03、D11、其它 P2 未修。 |
| IR03（独立修复副本） | 2026-10-28 | §8.2 区分终态发布失败与请求处理失败；终态可在同请求重放、启动及后台补发而不重发控制写；trace 修复未确认后缀与撕裂尾行。D11、其它 P2 未修。 |
| nullable-failsafe 兼容性 | 2026-10-28 | G8 与当前 API 对齐：允许省略／null／合法空报告，拒绝畸形报告；非空报告与其它安全字段仍独立限制写入。启动、发送前、发送后和恢复共用校验；无 journal 或事件格式变化。 |
| v0.11（审核修复） | 2026-10-29 | 依据审核 `REPAIR-ADVICE-2026-10-29-tier0-stage-1a.md`：§8.3 规定什么能解释"设备没收到前一个值"（只认同实例、环内在后、确认应用了另一个值的写入；FIX01），设备日志的覆盖与断链证明（FIX02），新增 `uncheckable_archive`、`uncheckable_device_log` 与结论 `INCOMPLETE`（退出码 2）。实验室假设备的日志每条带 `seq` 与 `mono`。§11.5 增修复后 runtime 上的 R3 实测，§11.6 的浸泡请求数按口径分开写。状态词汇、闸门、journal 与 trace 格式不变。 |
| v0.10（WP6、P1、WP9） | 2026-10-29 | §8.3 记录对账程序的实现与分类（"有 HTTP 答复却无条目"是差异；实验室外 `executed_unaudited` 不可判）；新增 §11.6（P1 与浸泡的实测事实）。状态词汇、闸门、journal 与 trace 格式不变。 |
| v0.9（R3 时序） | 2026-10-29 | 依据 §11.5：M10 的对账记录改为 `audit_entry`（`found` / `absent` / `unavailable`）加 `landed`（只在 `found` 时为 true，否则 null，**不再写 false**），旧版 `landed: false` 按 `absent` 读；§8.3 增"写入在客户端断开后执行而没留条目"；恢复时无条目的 `detail` 注明"不证明没有执行"。状态词汇、闸门、journal 与 trace 的其余格式不变。IA2 运行时漏记审计的修复不在本 PR。同日：§8.4 trace 启动读取对所有非桥记录的完整行一致跳过并报行号（此前非 ASCII 行、非对象 JSON 会以未捕获异常结束启动，非 JSON 行则静默跳过）。 |
