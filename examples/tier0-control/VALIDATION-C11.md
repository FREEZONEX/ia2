# C11 本机 MQTT 直写表征

**修复复验更新，2026-11-02 UTC+8：** 独立审核提出的 P2（断线被误认为关闭写权限有效）和 P3（端口被占用仍报告 broker 就绪）均已修复。最终本机复跑 **C10 9/9、C11 43/43**，六项真实进程回归通过；以下原 42/42 记录保留为首次执行历史。修复详情及新证据见文末“审核修复复验”。

2026-11-02 UTC+8，**OFFLINE PASS**。独立 runtime 二进制、临时 rumqttd broker、两个假 Modbus 设备经过真实 MQTT 链路，C11 **42/42** 检查通过。独立 C10 北向写保护回归 **9/9** 通过。未连接 grey0、真实 Tier0 或物理设备，未部署；A3b 不属于本轮任务。

最终运行时间：2026-11-02 **14:20:05–14:20:22 UTC+8**，其中 C11 为 **14:20:13–14:20:22 UTC+8**。完整记录保存在 [evidence/c11-20261102/](evidence/c11-20261102/)，多数 evidence 文件被 Git 忽略，不能只靠 clone 获取。下面链接指向本机保留的原始证据。

## 范围与结果

| 类别 | 输入／动作 | 实际结果 |
|---|---|---|
| 原始 C11 | `allow_write=true`，发送非 retained、QoS 1 的 `{"name":"sp_flow","value":12.5}`，控制桥与采样器均不启动 | 恰有一条对应写入审计；`origin=mqtt`、`op=write`、requested/applied 均为 REAL 位值 `1095237632`、result=`ok`；快照与假设备反馈均为 12.5，假设备收到 `[0x4148,0x0000]` |
| 治理增强：拒绝 | `unlisted_sp=5.0` | 审计 requested=`1084227584`，省略 applied 字段，结果为治理拒绝；变量不变，假设备没有收到该值 |
| 治理增强：限幅 | `sp_flow=50.0` | 审计 requested=`1112014848`、applied=`1109393408`、result=`clamped`；快照和反馈为 40.0，假设备收到 `[0x4220,0x0000]` |
| 响应边界 | 写入前订阅成功，收到 online 与 snapshot；三条写入后继续观察 1.5 秒并用 PINGRESP 检查连接 | 14:20:16.410–14:20:19.457 UTC+8，唯一前缀下只观察到 status、snapshot、write，没有逐请求结果／错误主题。PUBACK 单独记录为投递确认 |
| 关闭增强 | 正常停止自己的 runtime（退出 0），改为 `allow_write=false`，启动新实例，发送 `sp_flow=25.0` | 新 runtime_id、空审计、持续扫描与新 snapshot；独立观察器收到发布，随后 2 秒内无新增审计、设定与反馈不变，假设备没有收到 25.0 |
| 独立 C10 | 开启、缺省、关闭、不可读取／解析等现有北向写保护检查 | 9/9，通过；C11 自身不启动桥，也不发送任何桥签名请求 |

MQTT 载荷中的 REAL 是数值本身，不能直接使用 HTTP `/write` 的位编码。C11 绕过控制桥的签名、注册表等入口检查，但 runtime 治理仍执行。标签归因不构成身份认证。

## 实际修改

- `crates/runtime/examples/test_broker.rs` 及 Cargo example 声明：复用已有 `rumqttd` dev-dependency，拒绝非回环绑定，监听就绪后报告 READY，stdin STOP／EOF 退出；没有安装系统 broker，也没有修改产品 runtime 实现或依赖锁文件。
- `lab/test_broker.py`：显式选择 broker 二进制，限制就绪等待，收回自己启动的进程。
- `lab/minimqtt.py`：stdlib MQTT 3.1.1 测试客户端，检查 SUBACK／PUBACK、记录消息、支持 PINGRESP 活性检查并关闭接收线程。
- `lab/cases_c11.py`：独立 IDE 会话、真实 MQTT 写入、逐条审计差量、快照／回读、正常重启、关闭对照及退出清理。
- `lab/harness.py`、`lab/run_cases.py`：注册 C11、增加显式 broker／server／CLI 参数，允许本用例不启动桥与采样器；现有用例默认路径不变。
- `tests/test_c11_helpers.py`：5 项失败边界测试，防止拒绝订阅、缺少投递 ACK、失去连接、broker 启动退出／超时被误报为成功。
- README、TEST-CASES 与 RESULT-1A：补运行入口和本轮结果，保留历史未执行状态。原“无任何响应通道”措辞明确为有界观察中没有逐请求应用结果主题，不把正常遥测和投递确认排除在实际行为之外。

## 源码、环境与受测文件

工作目录为 `/Users/au73/Projects/soft-PLC/ia2-ir03-pr-20261028`，本轮收尾分支 `test/tier0-c11`，HEAD `61f695b1b72e9073086caeb97d90fdae6e041963`；ironplc 子模块 `80a1aa8bdaa8c1da948f0ff56c7b6f35913e9524`。14:19 UTC+8 在线读取的上游 main 与该 HEAD 相同。检出中原有未完成的 C11 草稿先保存在 [source-before/](evidence/c11-20261102/source-before/)，再接续修改。旧分支名 `test/tier0-a3b` 已更为 C11 专用分支；独立 A3b 检出未修改。

Python 使用 uv 已有的 **3.14.8**，项目 `.venv/bin/python`，无第三方 Python 依赖；没有向苹果开发工具 Python 安装依赖。Rust/Cargo **1.99.0**、pnpm **11.0.9**，目标 `aarch64-apple-darwin`。环境和锁文件摘要见 [baseline.json](evidence/c11-20261102/baseline.json)。构建和测试保留工作树 dirty 的事实；HEAD 本身不能还原新增文件。

| 最终发布版文件 | SHA-256 |
|---|---|
| runtime | `96da6883e4c403612a1654a69fe484ecf11aaf6de737378191292bbd27ada98f` |
| test_broker | `89a6af6eca30bce6c22d5e11061c8eb6ca86174ff82a2ce9ba82e28b0a8820a7` |
| server | `92284f1edcf0fc44cc7bcf1b8da81838a791255cb832c0744bd612cc135c3517` |
| cs | `42f569d7c0339affa9e4fa87f96fe9265cc7a54f11d60624fd478c3c00b1718d` |

四个文件在 C11 运行结束前再次计算并与起始配置比较，全部一致。最终 Python 运行代码摘要为 `acd806506f447aadf85bbf4d02f2466c550c04c640b130e61e6ad73bed2725f4`，范围以 verdict 的 `code_files` 为准，不等于整个仓库。源码归档、补丁、新增文件与二进制副本保存在 [source-final/](evidence/c11-20261102/source-final/)。

## 留证入口

| 原始资料 | 内容 |
|---|---|
| [acceptance.log](evidence/c11-20261102/acceptance.log) | 最终 C10／C11 运行摘要 |
| [C11 verdict](evidence/c11-20261102/acceptance/C11-r1/verdict.json) | 全部 42 条检查、错误字段、代码摘要和观察窗口 |
| [逐条矩阵](evidence/c11-20261102/acceptance/C11-r1/matrix.json) | 四次发送及其审计前后、独立快照 |
| [发送日志](evidence/c11-20261102/acceptance/C11-r1/mqtt-sent.jsonl)／[MQTT 消息](evidence/c11-20261102/acceptance/C11-r1/mqtt-messages.jsonl) | 未 retained 的原始载荷、投递 ACK 时间与观察结果 |
| [HTTP 读取](evidence/c11-20261102/acceptance/C11-r1/http.jsonl) | /health、/status、/audit、/alarms 和 IDE 请求／原始响应文本 |
| [设备 1](evidence/c11-20261102/acceptance/C11-r1/slave-1.jsonl)／[设备 2](evidence/c11-20261102/acceptance/C11-r1/slave-2.jsonl) | Modbus 请求、应答及写入 words |
| [起始配置](evidence/c11-20261102/acceptance/C11-r1/config.json) | 唯一 topic prefix、端口、所选二进制及摘要 |
| [开启配置](evidence/c11-20261102/acceptance/C11-r1/northbound-enabled.toml)／[关闭配置](evidence/c11-20261102/acceptance/C11-r1/northbound-disabled.toml) | 两个实际阶段使用的配置 |
| [正常停止](evidence/c11-20261102/acceptance/C11-r1/restart-stop.json)／[最终状态](evidence/c11-20261102/acceptance/C11-r1/final.json) | 重启前退出状态与收尾前快照 |
| [会话命令](evidence/c11-20261102/acceptance/C11-r1/exercise-command.json)／[退出](evidence/c11-20261102/acceptance/C11-r1/exercise-exit.json) | cs agent run 包装及 worker 退出码 |
| [清理](evidence/c11-20261102/acceptance/C11-r1/cleanup.json) | runtime、broker、IDE 的退出及监听端口释放 |
| [C10 verdict](evidence/c11-20261102/acceptance/C10-r1/verdict.json) | 独立桥保护回归 |
| [checks.jsonl](evidence/c11-20261102/checks.jsonl) | 质量命令、UTC+8 时间、退出码；各命令的完整日志位于同目录 |
| [Python summary](evidence/c11-20261102/unit/summary.json) | 每个套件的测试数与退出码 |
| [只读核对脚本](evidence/c11-20261102/verify_evidence.py)／[全量摘要](evidence/c11-20261102/MANIFEST.sha256) | 核对保存文件、受测二进制、归档还原的代码摘要及原始矩阵；不启动服务或访问硬件 |

broker、server、runtime 两个实例的日志都保留在 C11 用例目录。原生日志的 UTC+0 时间原样保存；正文和新增运行记录使用 UTC+8。记录中的随机测试密钥只服务本机实验，不是部署凭据。

可从仓库根目录运行 `.venv/bin/python -I -B examples/tier0-control/evidence/c11-20261102/verify_evidence.py` 独立核对保存数据。本轮核对通过；首次核对脚本误将拒绝审计的 applied 当成必有字段，改为按实际省略字段读取，原始数据未改，过程见 [verification-notes.json](evidence/c11-20261102/verification-notes.json)。此核对不等同于新的 MQTT 运行或独立人工审核。

## 开发失败与质量检查

第一轮 [dev/C11-r1/verdict.json](evidence/c11-20261102/dev/C11-r1/verdict.json) 为 **FAIL，3/42 失败**：worker 非零、拒绝写入后快照未等新扫描，以及把 inactive/acked 预置设备告警误判为未清告警。修正为等待扫描计数前进，并检查 active／acked；没有改变 MQTT 写入、治理或产品实现。第二轮 [dev/C11-r2/verdict.json](evidence/c11-20261102/dev/C11-r2/verdict.json) **42/42 PASS**。开发轮原始日志与 verdict 均保留；没有为每个中间版本保存完整源码归档，最终版本另有完整归档。

项目 AGENTS.md 的八项门禁全部退出 0：agent adaptation、fmt、workspace/all-targets Clippy `-D warnings`、server build、workspace tests、release build、Web build、Web tests。另有 broker release build、Python 回归和最终 diff 检查。

- Rust：**8,594 passed、105 ignored、0 failed**；ignored 不算执行通过。
- Web：**423 项、39 个文件通过**。
- Python：**371 项、31 个套件通过**，含新增 5 项 helper 测试。
- 既有真实 MQTT 底层测试 `governed_writes_over_mqtt_deny_clamp_apply_and_stay_silent` 单独复跑通过，见 [mqtt-unit.log](evidence/c11-20261102/mqtt-unit.log)。
- C11 会话内的实际工程及模拟工程编译检查通过；7 步 `cs sim run` 通过。它验证已有设定值语义，不宣称该仿真本身覆盖 MQTT 传输；MQTT 链由 C11 专项覆盖。

## 复跑与结果边界

从仓库根目录，使用新输出目录，且不占用别人的端口：

```sh
cargo build --locked --release -p server -p ia2-cli -p ia2-runtime
cargo build --locked --release -p ia2-runtime --example test_broker
IA2_LAB_PORT_BASE=25700 .venv/bin/python -I -B examples/tier0-control/lab/run_cases.py \
  --bin target/release/ia2-runtime --broker-bin target/release/examples/test_broker \
  --server-bin target/release/server --cs-bin target/release/cs \
  --out examples/tier0-control/evidence/c11-REPEAT --cases C10,C11
```

最终 C11 使用 runtime 25701、proxy 25702、假设备 25711／25712、broker 25783、IDE 60254。清理记录显示 runtime 两次均正常退出 0、broker 退出 0、IDE 经测试终止 -15；所有六个监听端口均已释放。收尾前无 force、fault、watchdog trip；预置设备告警均 inactive/acked。没有自动复位或确认告警。

本结论只覆盖此次本机链路和明确观察窗口；不证明其他前缀、任意时长内不存在消息，不涵盖真实网络可靠性、Tier0 适配、设备物理效果或断电持久性。没有新增长稳测试。以上为首次验收时点的记录；后续提交与 PR 状态以仓库历史为准，PR 默认交由 Mercy 合并。

## 审核修复复验

2026-11-02 **15:10:52–15:11:10 UTC+8**，C10 **9/9**、C11 **43/43**。新记录在 [c11-fix-20261102](evidence/c11-fix-20261102/)，原验收目录保持原样。复验输入为 `test/tier0-c11` 分支上基于 `61f695b1b72e9073086caeb97d90fdae6e041963` 的未提交工作树；此身份记录固定该次复验输入，不代表后续提交状态。

- P2：关闭写入的两秒观察要求非 retained 的 runtime snapshot 持续推进，边界及相邻推进快照间隔不超过 600 ms；核对 HTTP 两端 runtime_id、扫描与时间范围、反馈质量，并拒绝 offline/degraded。重启 online 只能来自本次重启请求之后，移除了回看三秒的旧条件。最终正常窗口收到 10 条推进快照，最大间隔 201 ms。
- P3：临时 broker 直接驱动 rumqttd 的 `Server::start`，优先处理本服务的绑定／运行结果；端口占用会退出 1，且不输出 READY。普通 TCP listener 和另一 MQTT broker 两种占用均已验证，既有 broker 保持可用。
- `tests/test_c11_helpers.py` 从 5 项增至 14 项。新增失联、降级后恢复、间隙、重复、retained、畸形与不匹配快照等边界。
- `tools/check_c11_regressions.py` 与 `tools/c11_dropout_worker.py` 保存真实进程回归。权限关闭／开启两种断线均由新增活性断言拒绝；权限开启且连接正常由写入审计、变量与设备结果拒绝。故障用例的内层 FAIL 是预期结果，外层六项回归全部通过。复跑命令见 [README](README.md)。

全量门禁本轮重新运行：Rust **8,594 passed、105 ignored**，Web **423 项／39 文件**，Python **380 项／31 套件**；Clippy、fmt、agent adaptation、server／release／broker 构建和 diff 检查均退出 0。见 [checks.jsonl](evidence/c11-fix-20261102/checks.jsonl)、[验收日志](evidence/c11-fix-20261102/acceptance.log)、[真实进程回归](evidence/c11-fix-20261102/regressions/summary.json) 和 [核对摘要](evidence/c11-fix-20261102/verification.json)。

代码摘要为 `fb43744139439fdefa2f58ba5521625842e8c5262f4b845b3dfaca568e831d05`，仍只涵盖 verdict 的 Python `code_files`。新 broker SHA-256 为 `31da63293612cf5f0e15043737d39f46c291714f388f9e6eaeadcd7e0d491c6d`；重新构建的 runtime、server、cs 与原封存摘要相同。完整当前修改文件、补丁、新增文件及工具链身份见 [source-final](evidence/c11-fix-20261102/source-final/)，受测文件见 [final-binaries](evidence/c11-fix-20261102/final-binaries/)。

正常验收及故障回归的 runtime、broker、IDE 已退出，监听端口与审核代理端口已释放；没有遗留 force、fault 或 watchdog trip。此次仍为本机模拟，无 grey0／真实 Tier0／物理设备操作，无 PR 合并。600 ms 是本测试在 200 ms 发布周期下的观察有效性上限，不改变产品的网络行为。
