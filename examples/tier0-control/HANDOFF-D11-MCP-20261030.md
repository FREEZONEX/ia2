# D11 与本地控制 MCP 复核交接

日期：2026-10-30 UTC+8。原复核范围：用户要求「把 D11 和控制 MCP 再核对一遍，做交接文档」；后续 main 集成与 PR 交付见首节。这是同一实现者的再次核对，不标作独立第三方审计。

## main 集成更新

2026-10-30 UTC+8：已将 `origin/main` `f0fa410bc8c22263aba7de0bc5400d0273d61c13` 合入，合并提交为 `9eaa24bea7e70f44aba8f655d29afc10e60032df`，包含 #100/#101。最新验证与 PR 交付状态见 [VALIDATION-D11-INTEGRATED.md](VALIDATION-D11-INTEGRATED.md)。下面的 365 项／16 类结果及“尚未合入 main／未推送”均为此前复核的历史状态；异常返回和操作边界仍适用。

本次集成验证为 **OFFLINE PASS**：366 项 Python、50 类回环／596 条断言、72 个变异检查、3 个崩溃恢复动作、7 步无设备模拟及全仓八项检查均通过。新增四类返回边界回归；修正一条与 main 新诊断措辞不一致的旧断言，保留首次失败证据。没有新增生产代码修改或发现。runtime 从合并后的树重新构建并固定散列；全部测试服务已退出，模拟结束时 `running=false`、`forces=[]`、告警 `[]`。

已推送并提交 [PR #102](https://github.com/FREEZONEX/ia2/pull/102)，目标 `main`，当前开放待审核、无合并冲突，尚未合并。已验证代码提交为 `2824b6a25bc0c878a9668b177787bcb07cacaeda`；之后仅补充 PR 状态文档。

## 此前复核结论与定位

本地实现包含事件／样本签名、独立只读采样进程，以及真实 stdio 控制 MCP。运行范围仍是本机回环与假设备；没有连接 grey0、真实 Tier0 或硬件，没有修改客户端 MCP 配置，没有推送本轮工作。

| 项目 | 定位 |
|---|---|
| 本机仓库 | `/Users/au73/Projects/soft-PLC/ia2-ir03-pr-20261028` |
| 本地分支 | `feat/tier0-signed-control-mcp` |
| 上一轮实现提交 | `25ac47ca92fe68ba6dc420a95ac138373a20f557`，开始复核时工作区干净 |
| 本轮代码提交 | `9c72dce`：签名 JSON 读取一致性、UTF-8 字节边界及三个新增回归 |
| 使用说明 | [LOCAL-CONTROL.md](LOCAL-CONTROL.md) |
| 上一轮验证 | [VALIDATION-D11.md](VALIDATION-D11.md)、[validation-d11-summary.json](validation-d11-summary.json)；历史数字不改写 |
| 本轮原始证据 | `evidence/d11-rereview-20261030/`，Git 忽略；包含修复前断言失败、修复后运行、冻结源码与散列 |
| 此前复核结论 | **OFFLINE PASS**；365 项 Python、16 类回环、5 个 D11 变异及全仓检查通过；现场项仍待验证 |

上游状态已重新通过 GitHub 查询：[#100](https://github.com/FREEZONEX/ia2/pull/100) 于 **13:10:14 UTC+8** 合并，合并提交 `88bcc6ec48c57510fa4a8fd8cb855ffc04a8cadf`；[#101](https://github.com/FREEZONEX/ia2/pull/101) 于 **13:10:45 UTC+8** 合并，合并提交 `f0fa410bc8c22263aba7de0bc5400d0273d61c13`。旧交接里「待合并」是历史状态。此次只 fetch 并核对，没有把 `origin/main` 合入本地 D11 分支；因此本地 Rust 源码仍不含 #100。对 `9c72dce` 与该 main 做的 `git merge-tree --write-tree` 预演无冲突，但未运行合并树。后续提交 D11 PR 前应先对齐 main 并验证合并后的树。

此前复核固定使用以下已含 #100 修复的二进制，当时本分支源码尚未合入 #100，不能直接替换为当时构建的 `target/release/ia2-runtime`。本次已合入 main 并重新构建；旧限制不适用于现已集成的树，新旧验证的固定二进制及散列分开记录：

```text
/Users/au73/Projects/soft-PLC/pr99-r3-timing-20261029/bin/ia2-runtime-detached-r2
SHA-256 c18b0aaab60425ef848e8e278b3ac60c1ec7a50dfb708ed25184a0a987018893
```

## 本轮发现与修复

| 编号 | 修复前现象及影响 | 修复与证据 |
|---|---|---|
| R-D11-01 | 有效事件 MAC 内的 `1e999` 被桥恢复读取器解析为 infinity 并接受，独立读侧却拒绝。同一行可能成为桥的去重 ID／journal 锚，却不能作为读侧证据。触发需要有效事件签名，不是无密钥伪造绕过。 | 桥签名和恢复检查均拒绝非有限 JSON 数值；不重新计算或规范化原始 payload 的 MAC。新增测试覆盖顶层／嵌套溢出和不采信锚、不抑制正常补发。 |
| R-MCP-01 | `json.loads(bytes)` 自动识别 UTF-16/32，使 stdio 服务接受约定之外的编码。 | 先严格解码 UTF-8，再解析 JSON。新增测试确认 UTF-16BE／UTF-32BE 被报为 parse error，之后合法 ping 仍成功。 |
| R-D11-02 | 桥允许日志里的 JSON 转义诊断字符串，但读侧对解码后的孤立代理项再次做 UTF-8 编码，拒绝原本按精确字节签过的事件。 | 保留日志原有的转义字符串语义。UTF-8 检查落在传输字节和被签的 payload 文本上；JSON 输出继续使用 ASCII 转义。新增往返／恢复测试覆盖转义代理项、中文、NUL、补充平面字符。 |

R-D11-01、R-MCP-01 的回归在修复前确实断言失败。R-D11-02 在冻结的 `25ac47c` 源码上复现为「桥接受、读侧拒绝」，修复后两边一致。中间一次过度严格的字符串检查触发了原有 `test_journal_and_trace_take_any_string`；该失败保留在 `unit-reviewed/`，最终实现保留了原测试要求，未删断言。

这里区分「字节编码」与「JSON 字符转义」。RFC 8259 §8.1 要求系统间 JSON 使用 UTF-8；§8.2 指出语法可包含孤立代理项转义，跨实现的行为可能不同。本例保留原有日志诊断契约，并不据此声称任意外部消费者均兼容；后续 Tier0 适配必须保留精确 payload 字节，不经过有损转码。[RFC 8259](https://www.rfc-editor.org/rfc/rfc8259)

## 已核对的执行与信任边界

```text
Agent ── stdio MCP ── requests.jsonl（请求 HMAC）── bridge ── /write ── 本地 runtime
                         │                          │
                         │                   trace.jsonl（事件 HMAC）
                         │                          │
                         └── get_action_status ─────┤
                                                    │
独立 sampler ── GET /status ── samples.jsonl（样本 HMAC）┘
```

| 核对点 | 当前行为／接手位置 |
|---|---|
| 三种签名域 | 请求 `ia2.control/v0`、事件 `ia2.event/v0`、样本 `ia2.sample/v0`；HMAC-SHA256 覆盖域前缀、换行和精确 payload UTF-8 字节 |
| 强制验签 | 角色密钥、撤销、签名内时间窗、hint、schema/kind 均核对；拒绝记录计数；公共 verifier 没有无签名模式。见 [readside/auth.py](readside/auth.py)、[readside/verifier.py](readside/verifier.py) |
| 恢复 | 只有可信事件才能提供去重 ID／journal 锚。终态发布重试只补发结果，不重复 runtime 写。见 [bridge/eventauth.py](bridge/eventauth.py)、[bridge/trace.py](bridge/trace.py)、[bridge/engine.py](bridge/engine.py) |
| 采样 | 独立进程、独立样本密钥，仅 GET 回环 `/status`；默认 200 ms；非有限反馈编码为 null，保留原始 bits；输出有单写者锁。见 [readside/sampler.py](readside/sampler.py) |
| MCP 工具面 | 只有 `ia2_set` 和 `get_action_status`。启动固定注册表 SHA-256 和文件路径，拒绝启用的非 simulation 目标。没有任意变量、地址、force、pulse、文件路径或执行命令参数。见 [control_mcp.py](control_mcp.py) |
| 查询 | 从验签后的请求恢复 action_id 绑定，验签事件／样本后推导；重启 MCP 不创建新请求。`requested` 不是执行成功，`accepted` 不是反馈达标，`verified` 不是物理效果或因果证明 |
| 异常 | 按发生层级区分信封拒绝计数、推导结果 `unknown` 与 MCP 工具错误，详见下表；请求追加／同步不确定时返回 action_id + `unknown`，禁止自动重投 |
| 包装 | [tools/package_prod.py](tools/package_prod.py) 包含桥、读侧、采样器与 MCP，去掉 lab hooks；`prod` 命名不是部署验收 |

`simulation: true` 是注册表约束，不是对 runtime 接线的自动证明。此次用例确实由 harness 创建回环假设备。接手者仍须保证自己的 runtime 项目也是模拟项目。

### 异常返回的判读

`get_action_status` 的动作状态与 MCP 的 `isError` 是不同层级：`isError: false` 只表示工具正常返回结果，不能据此判断动作成功，也不能据此排除证据异常。

| 发生位置／条件 | 当前返回行为 |
|---|---|
| 事件／样本信封未通过签名、撤销、JSON 或基础结构检查 | 忽略该行并增加 `rejected_envelopes`／`rejected_by_role`；继续用其余可信证据推导。查询正常完成时为 `isError: false`，动作状态取决于剩余证据，并非统一报工具错误。 |
| 已验签证据在 verifier 推导阶段触发其内部捕获的异常 | 返回 `status: unknown`、`reason: no_proof_by_deadline`、`detail: malformed authenticated evidence`，MCP 为 `isError: false`；该推导异常不增加信封拒绝计数，因此计数仍可能为 0。例：非 `accepted` 终态分支中，两条 `reconcile_note` 的 `pass_no` 分别为字符串和整数，排序时触发类型异常。 |
| 查询外层的请求恢复、注册表绑定、证据读取／筛选等触发 MCP 捕获的异常，或超过本地历史／大小预算 | 返回 `isError: true` 的工具错误。`KeyError`／`TypeError`／`AttributeError`／`OverflowError` 为 `local_data_error`，`OSError` 为 `local_io_error`，`ValueError` 或注册表错误为 `tool_refused`；stdio 仍可继续服务。 |
| `ia2_set` 在请求日志追加／同步期间发生 I/O 异常，无法确认请求持久化结果 | 返回 action_id、`status: unknown`、`reason: request_persistence_unconfirmed`，MCP 为 `isError: false`；请求可能已可见，应查询同一个 action_id，禁止自动重投。 |

客户端应同时检查 `isError`、动作 `status`、`reason`／`detail` 与拒绝计数；工具错误也不能作为「动作未执行」的证明。实现边界见 [readside/verifier.py](readside/verifier.py) 的 `derive` 和 [control_mcp.py](control_mcp.py) 的 `get_action_status`／`Server.dispatch`。

## 密钥、权限与恢复操作

| 进程 | 持有的秘密 | 写入范围 | `--revoked-keys` 生效范围 |
|---|---|---|---|
| bridge | 请求验证密钥、事件签名密钥 | journal、trace、运行状态文件；受约束的 runtime 写入 | **事件密钥** |
| sampler | 样本签名密钥 | samples | 样本密钥 |
| MCP | 请求签名密钥、事件及样本验证密钥 | requests；读取 trace/samples | 请求、事件、样本密钥 |

三角色用不同秘密和不同 key ID，配置文件 0600，状态目录建议 0700。所有配置都在启动时读取；修改后重启相应进程。不要复用秘密来“轮换” key ID，也不要把秘密放进工具参数、提示词或版本库。

特别注意：bridge 的 `--revoked-keys` **不会撤销请求密钥**。要停止桥接受某请求密钥，必须从桥的 `--keys` 密钥环移除该 ID 后重启，同时更新 MCP 配置。撤销不会撤回已经发送或进入持久化恢复流程的动作。正常到期的历史证据可凭原时间窗继续验证；明确撤销的证据不再可信。

HMAC 验证方也能签名，因此 MCP 持有的三种秘密使其处于完整信任边界内。同一 OS 用户的三个文件不能隔离进程。签名抵御没有密钥的伪造／篡改，不能证明签名者报告的事实正确，也不防删除或拒绝服务。既有七工具只读 MCP 未改，本轮没有审核其线上部署。

出现 `request_persistence_unconfirmed` 时先查同一个 action_id，不能重新调用 `ia2_set` 生成新 ID。若连接在返回 ID 前断开，应核对持久化请求与审计后再决定，不能把超时当作未执行。`pending_trace_results` 不为零时优先核对事件密钥窗口、撤销和文件错误，不要重发控制写来“修复”结果发布。

## 本轮验证与证据

以下是本轮重新执行的结果，机器可读记录见 [validation-d11-rereview-summary.json](validation-d11-rereview-summary.json)。上轮 50 类覆盖、64 + 3 个变异及历史两小时浸泡未在本轮整体重跑。

| 检查 | 本轮结果 | 证据 |
|---|---|---|
| 原提交基线 | 362 项 Python、5 个 D11 变异、F6/F7/F8/MCP/MCPPROD 均通过 | `unit-baseline/`、`mutations-baseline/`、`integration-baseline/` |
| 修复后完整 Python | **365 项／30 套件通过** | `unit-reviewed-final/` |
| 修复后 D11 变异 | **5/5 被断言识别** | `mutations-reviewed-final/` |
| 修复后回环与独立包 | **16/16 PASS** | `integration-reviewed-final/` |
| 仓库八项质量检查 | **全过**；Rust 8,589 通过／105 忽略；Web 423 项／39 文件通过；格式、Clippy、构建均 exit 0 | `reviewed-final-runs.json` 及各命令日志 |
| 无设备模拟场景 | **7/7 步骤通过**；project check/open 通过，最后 running=false、forces=[] | `sim-reviewed/` |

本轮回环集：`A1,B2,R1,R2,R3,R3T1,R3T1K,R3T1X,R3T2,P1F5,F6,F7,F8,MCP,MCPPROD,PROD`，涵盖正常闭环、溢出拒绝、崩溃恢复、断连时序、非有限反馈、伪造事件／样本、真实 MCP 重启与独立包。上轮更广的 50 类覆盖及历史失败／复跑链条仍见 [VALIDATION-D11.md](VALIDATION-D11.md)。

本轮源码与冻结副本的 104 个 Python 文件及词汇表逐项 SHA-256 一致，读侧仍无 bridge 导入。`final-code-manifest.json` 固定代码，`reviewed-source-final.json` 固定执行前的源码快照，`EVIDENCE-INDEX.sha256.json` 索引结果与各用例清单。

修复后的 CPU 微测量（Python 3.9.6，3 轮、每轮 5,000 次）：事件签名约 10.88 µs、事件验签 15.06 µs、样本签名 18.74 µs、样本验签 38.69 µs；一条事件加四条样本的验签与推导约 184.94 µs。5 Hz 的样本签名加一次验签约占一个核的 **0.0287%**，不含 `/status`、HTTP、磁盘、IPC 或额外查询；MCP 每次查询仍需验签选中的证据。原始记录为 `microbenchmark-reviewed-final.json`，不是现场负载结论。

## 最短复跑入口

以下使用 main 集成轮固定的 runtime，全部命令从仓库根目录运行；具体 SHA-256 与构建来源见 [集成验证](VALIDATION-D11-INTEGRATED.md)。其他机器可按 [LOCAL-CONTROL.md](LOCAL-CONTROL.md) 从已集成源码重新构建。输出目录必须是新的；示例名称若已存在则换一个。Python 环境沿用项目现状，无第三方包安装要求。

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py \
  --out examples/tier0-control/evidence/handoff-unit-new
python3 -I -B examples/tier0-control/tools/check_d11_mutations.py \
  --out examples/tier0-control/evidence/handoff-mutations-new
python3 -I -B examples/tier0-control/lab/run_cases.py \
  --bin examples/tier0-control/evidence/d11-pr-20261030/bin/ia2-runtime-integrated \
  --out examples/tier0-control/evidence/handoff-mcp-new \
  --cases F6,F7,F8,MCP,MCPPROD
```

这些回环用例共用一组端口，应串行运行。手动启动三进程、生成密钥、固定注册表和运行打包版的完整命令在 [LOCAL-CONTROL.md](LOCAL-CONTROL.md)，不要用测试夹具中的固定密钥配置日常会话。

## 剩余事项与下一步

| 状态 | 项目 | 关闭条件 |
|---|---|---|
| 已提交 | main 集成与 D11 PR | [PR #102](https://github.com/FREEZONEX/ia2/pull/102) 开放待审核；集成验证通过，尚未合并 |
| 本地限制 | 每个日志最多索引 20,000 条（包括坏行） | 5 Hz 约 67 分钟；超限报工具错误。重启 MCP 并不会减少同一日志的行数。长期运行需持久索引或完整会话轮换 |
| 未实现 | 真 Tier0 HTTP/MQTT 适配、服务部署 | 明确传输契约、权限范围与故障处理后实现；当前只是签名文件通道 |
| BENCH PENDING | 采样负载与 #65 | 实测目标机器新增 `/status` 负载、扫描周期与查询延迟 |
| BENCH PENDING | 硬件与反馈真实性 | 读回设备身份／映射，核对物理反馈、总线时序、断线及恢复行为 |
| BENCH PENDING | 断电持久性 | 实机断电恢复验证；不能用进程崩溃测试替代 |

会话轮换应在没有未决动作后封存整套请求／journal／trace／samples，再使用新的完整状态路径。不要仅截断某个证据文件来消除上限，也不要把旧请求文件和新 journal 混用。历史文件是审计证据，不是可直接复用的运行状态。

本轮创建的回环 runtime、桥、采样器、MCP 和模拟服务器均已退出；模拟最终 `running=false`、`forces=[]`，没有留下测试服务。模拟服务器对用户持久化 IDE 目录的读写被隔离，未改变用户的服务或项目运行状态。旧 P2 的 C11、A3b 等边界仍沿用 [阶段交接](HANDOFF.md)，没有因本轮 D11 复核而关闭。

接手者第一步读本文件和使用说明，核对代码及 runtime 散列；进入任何现场工作仍需用户对该阶段的明确授权。
