# A3b IDE 代理标签验证

日期：2026-11-02 UTC+8。结果：**OFFLINE PASS**。原始 A3b 的最小范围已关闭；C11 本轮未运行，独立跟踪。

## 结果与范围

在 main `61f695b1b72e9073086caeb97d90fdae6e041963`、ironplc `80a1aa8bdaa8c1da948f0ff56c7b6f35913e9524` 基线上，只修改测试及文档。最终使用本检出构建的发布版二进制：A3 为 4/4 检查通过；A3b 为 166/166 检查通过。runtime 的构建身份带 `-dirty`，对应本轮测试／文档修改，原始源码、补丁与新增文件见证据快照。

| 输入 `X-IA2-Origin` | 直连审计 `origin` | IDE 代理审计 `origin` |
|---|---|---|
| `tier0.lab.1234abcd` | 原样 | 原样 |
| `lab.a-b_c` | 原样 | 原样 |
| 64 个 `L` | 64 个 `L` | 64 个 `L` |
| `lab:colon` | `lab:colon` | `labcolon` |
| `lab space` | `lab space` | `labspace` |
| 70 个 `L` | 70 个 `L` | 64 个 `L` |
| 空字符串 | `anonymous` | `anonymous` |
| 缺省头 | `anonymous` | `anonymous` |
| `!!!` | `!!!` | `anonymous` |

原始 A3b 最小范围是合规 Tier0 标签经过真实代理链路并与直连一致；完整九标签矩阵是扩展表征。每次使用可区分的值，发送下一条前读取 runtime 的审计前后差量及推进扫描后的变量快照；检查恰一条新审计及 `op/origin/name/requested/applied/result`，且无标准 HTTP `Origin` 时没有 `http_origin`。REAL 请求为 i32 位编码。

另有两组**增强覆盖**，不追溯扩充原始要求：REAL 超出治理上限得到 200、应用 40.0、审计 `clamped`；无允许规则的 DINT 变量得到 403、完整治理拒绝文本、审计无 applied 且变量不变。SSH curl 的原始响应体与状态码均和 IDE 返回逐项比较。22 次写入中 11 次经代理，恰有 11 个 SSH 调用及 11 个 recording proxy 写请求。

复制的无设备工程通过 `cs project check`，既有 `sim/scenarios/setpoints.toml` 的 7 步也通过；这些命令和标签矩阵一起运行在 `cs agent run` 内。两份 runtime 的审计彼此独立：模拟场景使用 IDE 本地模拟器，标签测试使用独立 runtime 的 edge 代理接口，未混用本地变量写接口。

## 可复跑入口

从仓库根目录运行（Python 标准库，无新增依赖；POSIX 环境需 `/bin/sh` 和 `/usr/bin/curl`）：

```sh
cargo build --locked --release -p server -p ia2-runtime -p ia2-cli
IA2_LAB_PORT_BASE=24500 python3 -I -B examples/tier0-control/lab/run_cases.py \
  --bin target/release/ia2-runtime --server-bin target/release/server \
  --cs-bin target/release/cs \
  --out examples/tier0-control/evidence/a3b-NEW-RUN --cases A3,A3b
```

必须选择新的输出目录；端口占用时拒绝启动，改用另一组 `IA2_LAB_PORT_BASE`。A3 使用该组的 runtime／proxy 和两个假 Modbus 端口；A3b 只用 runtime／proxy，并动态分配 IDE 端口。

`lab/cases_a3b.py` 复制 `sim/` 到证据目录，配置唯一工程名、临时 edge 和明确的 `runtime_port`。IDE 仅打开这一个工程，每个请求带 `X-IA2-Project`。IDE 子进程的 HOME、XDG 配置目录和工作目录均位于测试目录，且实际核对 `last_opened` 写入隔离目录。未关闭 Host／Origin 保护。

`lab/fake_ssh_a3b.py` 只对该 IDE 子进程出现在 PATH；不回退真实 SSH。它严格校验 `ssh_cmd` 选项、`a3b@must-not-resolve.invalid`、本轮回环端口和 POST `/write` 命令语法，保存原始参数／命令后用绝对路径执行原始 shell 文本。不重算标签，不伪造响应或审计；未知 systemd／部署／隧道命令会被拒绝。curl 使用隔离配置目录及空 PATH，参数中的引号语义保持不变。4 个边界单元测试覆盖允许命令及错误目标、端口、路径、额外命令和未引用 shell 展开等拒绝向量。

## 原始证据

本地证据根目录：`evidence/a3b-20261102/`（gitignored，不会随 clone 自动取得）。

| 文件／目录 | 内容 |
|---|---|
| `baseline.json` | 检出、上游 main、子模块、Rust／Cargo／Python／pnpm 身份与初始工作树 |
| `acceptance-command.json`、`acceptance.log` | 最终完整命令、端口环境变量、UTC+8 时间、退出码及结果 |
| `acceptance/A3-r1/` | 同轮原始直连 A3 对照、日志、最终状态和哈希清单 |
| `acceptance/A3b-r1/config.json` | 测试配置、进程命令、限定环境及 server／runtime／CLI 哈希 |
| `acceptance/A3b-r1/http.jsonl` | 原始请求与响应；每条审计前后读取及独立变量状态 |
| `acceptance/A3b-r1/ssh.jsonl`、`wire.jsonl` | SSH 参数、原始 curl、真实响应／退出码及实际进入 runtime 的代理请求 |
| `acceptance/A3b-r1/matrix.json`、`verdict.json` | 逐行对照、完整审计增量和明确断言 |
| `acceptance/A3b-r1/project-check.json`、`sim-smoke.json` | 工程检查与模拟场景的命令、输出及退出码 |
| `acceptance/A3b-r1/runtime-1.log`、`server.log`、`final.json` | 服务原始日志、健康／状态／审计终态；日志原生 UTC 时间戳原样保留 |
| `acceptance/A3b-r1/cleanup.json` | runtime 正常退出 0；IDE 被本测试终止；runtime／proxy／IDE 端口均释放 |
| `checks.jsonl`、各检查 `.log`、`unit/summary.json` | 项目质量门禁和 Python 回归的命令、退出码及计数 |
| `source/`、根 `MANIFEST.sha256` | 显式 HEAD 源码、子模块归档、最终补丁、新增文件及证据总哈希索引 |
| `pre-migration/`、`dev/`、`migration.json` | 开发失败、修正及隔离迁移历史；不作为最终验收替代 |

最终发布版 SHA-256：

| 二进制 | SHA-256 |
|---|---|
| `ia2-runtime` | `e648cfd2a68c7a5fec0dd7d34d73fdb2fbea712f72966a467c5495000462e9cc` |
| `server` | `a672adb95318e82336c1a2490f94dffdd0316c9e7b3cd9a8665f4cdaaa12832b` |
| `cs` | `e48620c2a873df869a954cc98bd80927dbe887b3c4f06066a524e94d71f8c441` |

## 历史、检查与证据边界

最初推荐检出在检查时干净，随后出现其他作业的 C11 文件。本轮将 A3b 修改移至独立检出 `ia2-tier0-a3b-20261102`，撤回旧检出中仅属于 A3b 的修改；保留 C11 文件。新检出拥有独立构建目录和 Git 对象，不依赖旧检出的对象存储。迁移后的构建、检查及最终运行才是本轮验收依据。

开发轮先因夹具误将 runtime `/status` 当成含布尔 `running` 的响应而在就绪检查停止；实际字段是 `mode.kind`。修正后，增强项的拒绝审计断言补全了代码已有的 `(write_mode = allowlist, no rule allows it)` 原因文本。以上均未改变产品行为、合规标签判据或既定非合规标签预期。失败和修正记录保留；未用进程退出 0 替代逐条断言。

质量检查包括三个已有标签单元测试、agent adaptation、Rust 格式／clippy、debug server 构建、Rust 全工作区测试、四项发布构建、前端构建及测试、Tier0 Python 套件。全部退出 0；Python 共 370 项、31 个套件通过，包含新增的 4 个假 SSH 边界测试。精确退出码及计数见 `checks.jsonl` 和 `unit/summary.json`。

本轮没有连接 grey0、部署、真实 Tier0、真实 SSH 网络或物理设备，也没有改变标签清理、HMAC 决策或产品代码。标签依然是自我声明归因，不是认证；本机 PASS 不能扩展为网络或硬件验收。以上记录原始验收；后续审核与修复见下节。C11 保持独立任务，真实环境证据不在本轮范围。

## 审核 P2 修复与重新验证（2026-11-02 UTC+8）

独立审核发现：原清理逻辑只用 `child.wait(5)` 等待 `cs` 包装进程；包装进程退出后，即使同组后代忽略 `SIGTERM`，也不会进入 `SIGKILL` 分支。正常矩阵 PASS 不覆盖这条异常路径。

`lab/cases_a3b.py` 新增 `stop_exercise_group`，由会话运行的 `finally` 调用。它先向本测试通过 `start_new_session=True` 创建的进程组发送 `SIGTERM`，在单调时钟计时的 5 秒宽限期内检查整个组，并回收直接子进程；期满后对仍存在的组发送 `SIGKILL`。包装进程提前退出不再跳过升级清理。已经退出的组正常返回；后代由操作系统回收，测试进程只直接回收自己的子进程。

新增 `tests/test_a3b_cleanup.py` 的 3 项真实进程回归：包装进程已退出但后代忽略 `SIGTERM`、等待包装进程超时且后代忽略 `SIGTERM`、整组已正常退出时保留成功退出码。测试通过后代继承管道的 EOF 确认它结束，避免把包装进程退出或尚未回收的僵尸 PID 当作运行状态。测试宽限期缩短为 0.1 秒，实际 A3b 仍为 5 秒。旧逻辑 3 项中 2 项失败，修复后 3/3 通过。

2026-11-02 14:00:11 至 14:02:36 UTC+8 重新运行的 11 项命令全部退出 0：agent adaptation、Rust 格式检查、clippy、server 构建、全工作区测试、四项发布构建、前端构建和测试、Python 全套回归、A3/A3b 验收及 diff 检查。Python 为 **32 个套件、373 项测试通过**；前端为 **39 个文件、423 项测试通过**；Rust 日志结果块合计 **8,594 passed、105 ignored**，ignored 不计作执行通过。

端口基数使用 24700，新证据根目录为 [`evidence/a3b-p2-fix-20261102/`](evidence/a3b-p2-fix-20261102/)。A3 **4/4 PASS**，A3b **166/166 PASS**；保存的 22 次写入、11 次 SSH/curl 与 11 次代理请求通过额外矩阵核对。最终 runtime 退出 0，IDE 由测试终止为 -15，全部测试端口释放。三个产品二进制的哈希与上文原始验收一致；修改后的测试代码树摘要为 `e0031f7bc45e83e0c6b920f8f84fe6099bb22fc3a6cb28924da698118247156e`（沿用 harness 的代码文件范围，不包含测试文件；新增回归源码另行保存）。

证据入口：[`checks.jsonl`](evidence/a3b-p2-fix-20261102/checks.jsonl)、[`verification.json`](evidence/a3b-p2-fix-20261102/verification.json)、[`unit/summary.json`](evidence/a3b-p2-fix-20261102/unit/summary.json)、[`regression-before.json`](evidence/a3b-p2-fix-20261102/regression-before.json)、[`regression-after.log`](evidence/a3b-p2-fix-20261102/regression-after.log)。旧逻辑失败记录为工具输出摘要，旁边保留提取为 helper 后、修复前的源码；修复后源码和文档存于 `source/`，总清单为 `MANIFEST.sha256`。原始审核包与原始验收证据未覆盖。本次 P2 已修复；没有新增硬件或真实 SSH 验收结论。本次修复随 A3b 测试与文档一并提交，PR 交由 Mercy（`M3rcyzzz`）审核并合并。
