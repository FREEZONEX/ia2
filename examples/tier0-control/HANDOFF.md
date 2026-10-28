# IR03、运行时兼容性与 D6 检查修复交接报告

日期：2026-10-28 UTC+8。

IR03、`failsafe_status: null` 兼容性及随后复查发现的 **D6 TOML 检查绕过**均已修复。修复后的完整回环联调通过。[PR #99](https://github.com/FREEZONEX/ia2/pull/99) 待审核，**未合并、未部署**。

## 交付定位

| 项目 | 内容 |
|---|---|
| 仓库 / 目标分支 | `FREEZONEX/ia2` / `main` |
| PR 分支 | `codex/tier0-ir03-publication-recovery` |
| 累计参考实现与 IR03 提交 | `cf4c41273bc91b05202a7483425cebe349f947f2` |
| 兼容性修复提交 | `24dc9dd61409a8dc4185dc34e9291bed9dce85f8` |
| D6 修复定位 | 本报告所在提交；`bridge/northbound_guard.py` 与 `tests/test_d6_toml.py` |
| 代码入口 | [bridge_run.py](bridge_run.py)、[bridge/](bridge/) |
| 运行与打包说明 | [README.md](README.md) |
| 验证详情 / 机器可读结果 | [VALIDATION.md](VALIDATION.md)、[validation-summary.json](validation-summary.json) |

仓库原先没有跟踪这套独立桥代码，因此 PR 包含累计 P0/P1、IR01/IR02、AR01、IR03 参考实现及其测试。仓库原有 Rust/Web 实现未修改；原工作区和各轮审核、修复副本保留。

## 修复结果

**D6：完整解析 TOML，关闭误放行。** 旧检查逐行搜索 `allow_write` 并截断 `#`，会漏掉转义键名，以及内联表字符串内 `#` 后面的写权限字段；实际运行时却会启用 MQTT 写入口。本轮以固定版本 Tomli 2.2.1 解析实际 `mqtt.allow_write`：

- 缺文件、缺 `mqtt` 表或省略字段按运行时默认 false 处理；显式值必须为布尔 false，即使 `mqtt.enabled` 为 false 也不豁免。
- 非法 TOML、错误表类型或非布尔字段均拒绝；错误消息不回显配置内容。
- 启动和每次新请求使用同一检查；已保存请求的重放仍返回原结果，不重新发送。
- 生产包包含相同的解析器、MIT 许可证及来源哈希，无需额外安装依赖；证据源码树哈希递归覆盖解析器。
- 同步了 `/audit` 单独恢复场景的旧注释：恢复时的扫描标记之后仍可用独立反馈验证，运行行为未改。

新回归套用旧检查器产生 **18 个断言失败、0 个错误**；修复后通过。两种原绕过均由真实 IA2 运行时确认启用 MQTT 写入口，修复后的源码版和生产包都在启动时拒绝；合法关闭配置正常启动。运行中修改配置的进程回归同时确认没有控制 POST。

**IR03：已保存的结果能够补发。** 以前 journal 已保存 terminal 后，trace 发布失败会进入请求异常处理，将有效请求隔离，导致重放也不能得到结果。现在同请求重放、启动恢复及后台例行步骤均可补发已保存的同一事件；不再次读取运行时、不重发控制 POST、不重新分类、不追加另一条终态。旧版已误隔离、超过热索引期限的动作也可恢复。真实请求处理异常仍按原规则隔离。

trace 短写会继续写完；失败后重试先移除未确认后缀；启动保留撕裂尾行副本，并同步完整行后再据事件 ID 去重。事件 ID、状态及业务证据保持不变，journal 锚可更新。消费者仍须按事件 ID 去重，不承诺消费者从未读到未同步行。

**运行时兼容性：接受合法的 null，保留独立限制。** 当前 API 用 `failsafe_status: null` 表示尚无 failsafe 尝试。旧桥把它当成畸形对象，启动自检因此失败。本轮将共享校验与 API 对齐：

- 允许旧运行时省略该扩展、当前运行时返回 null，或返回三个列表均为空的合法对象。
- 非 null 对象仍必须恰含 `pending`、`acknowledged`、`unconfirmed` 三个列表；任一列表非空仍阻止写入。
- null 不覆盖另一端或前一次快照的报告；watchdog、锁存、故障、设备健康、必需字段仍独立检查。
- `false`、`0`、空字符串、错误对象等仍拒绝；没有关闭 preflight，没有改变 journal 或事件格式。

关键审核位置：[D6 检查](bridge/northbound_guard.py)、[D6 回归](tests/test_d6_toml.py)、[runtime_schema.py](bridge/runtime_schema.py)、[兼容性回归](tests/test_failsafe_compat.py)、[IR03 回归](tests/test_ir03_publication.py)。

## 验收记录

| 验证 | 结果与范围 |
|---|---|
| Python 单元与进程测试 | **253 项通过**，含 14 项 IR03、7 项兼容性回归、6 项 D6 回归；D6 在 Python 3.9.6 与 3.14.8 上均通过 |
| D6 修复前对照与真实运行时检查 | 旧代码触发 18 个断言失败；源码版、生产包的 4 项拒绝与 2 项正常启动检查均通过 |
| 完整回环联调 | **26 PASS + 1 个已知 R3 FINDING**；395 条断言全部通过，无启动错误 |
| 逐动作崩溃恢复检查 | 通过，检查每个动作的结果与发送次数 |
| BOOL 假设备验收 | **21 项通过**，核对 POST、审计、快照和线圈结果 |
| 新增故障注入对照 | **3/3 检出**：恢复 null 拒绝、忽略另一端非空报告、错误接受畸形假值 |
| IR03 独立验收对照 | 先前 8/8 检出，实际旧版引擎/进程升级恢复、12 项复制复核通过；本轮重新运行仓库内 14 项 IR03 回归 |
| 仓库级检查 | 本轮重新运行适配、格式、Clippy、Rust 全套测试、发布构建及 Web 构建/测试，全部通过 |
| `cs` 无设备模拟 | 本轮项目编译及 7 步场景通过，结束时程序停止、无强制值、无报警；测试服务已关闭 |

运行时源码与 `main` 基线 `e6a38aa355cde1747114866b52c2e26f0e3ca724` 相同；本轮二进制由 PR 修复前提交 `047e7c17ffda1c52ac3d0f68726d9a15889a3cba` 构建，构建标识为 `047e7c17ffda`。本轮只修改 Python、打包和文档，没有修改该二进制的源码或依赖锁文件。

- 运行时二进制 SHA-256：`52160ab8ba63cd16852c66b5647f0a16844bdc45f410c48da2c589c2cc2e74de`。
- 最终 bridge/lab 源码树 SHA-256：`4b79f02cbca51474192941cf17496ca49f2a121451a66158e7ceea2225cb0e2b`，27 份联调 verdict 均匹配，包含内置解析器。
- 本轮原始日志：忽略入库的 `evidence/d6-fix-20261028T223623p0800/`、`evidence/process-d6/` 及 `evidence/bool-device-4/`。源码与汇总入库；运行时二进制、生成密钥及本机原始日志不上传。上一轮证据保留。

## 复跑

仓库根目录执行，Python 3.8+，所需 TOML 解析器已随代码携带，无需安装依赖；本轮全套使用 Python 3.9.6，D6 另用 3.14.8 验证，均为 macOS 本机回环环境：

```sh
python3 -I -B examples/tier0-control/tools/run_tests.py
python3 -I -B examples/tier0-control/tools/check_failsafe_controls.py
```

完整运行时、崩溃和 BOOL 联调命令见 [README.md](README.md)。这些联调用例共用固定回环端口，应串行执行，并为每次运行指定新的证据目录。

## 剩余边界与当前状态

- **R3 仍保留 FINDING**：运行时暂停期间未应答的写入仍是不确定结果；超时不能证明“没有执行”。
- **D11 事件／样本签名未实现**；当前 verifier 依赖可信本地证据。真实面向 Agent 的控制 MCP 仍未实现。
- **真实硬件和断电持久性未验收**：本次只使用假设备或无设备模拟，不能据此声称实际物理效果、接线正确、硬件时序或断电落盘保证。
- 测试服务已停止，测试端口已释放。PR 已具备代码审核条件；合并与部署没有执行。
