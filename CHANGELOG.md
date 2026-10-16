# 变更日志

本文件记录**对外可见**的变更（对使用者的行为有影响的一切）。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)；
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。
`.pbt` 格式版本（`FORMAT_VERSION`）与 C ABI 版本（`PB_ABI_VERSION`）
与包版本**独立演进**，任何一处变动都会单独标注。

## [Unreleased]

### 变更
- 收紧 Dependabot 策略：所有生态**显式忽略 major 升级**（major 属于决策，走 ADR），
  GitHub Actions 与 npm 各自分组、限制并发 PR 数。原因：首次推送后 Dependabot 在两分钟内
  开了 10 条大版本 PR（react 18→19、typescript 5→7、actions/checkout 4→7 等），
  全部需要人工评估，噪音会淹没真正要处理的事。

### 计划中（M1）
- `pinbridge.dll` 接通 Intel Pin：JIT 探针、上下文控制、TF 虚拟化。
- agent 内嵌 CPython 3.x：脚本加载/替换/卸载与执行时间预算。
- 真实内存读写、断点、单步、系统调用 Hook。

## [0.1.0] - 2026-10-16

M0：**工程骨架**。目标不是功能完备，而是"把规范、边界与验证方式先钉死"。

### 新增（M0.5：Hub ↔ agent 回环链路）
- `pinbridge-agent/src/link.rs`：目标侧的**回环链路服务端**。它不自己判断权限，
  而是转发给 `exports::pb_*`——两条入口（C ABI 与回环链路）因此不可能对
  "该不该放行"产生分歧。只监听 `127.0.0.1`（没有地址参数，不是默认值而是设计）。
- `pinbridge-proto/src/agent_link.rs`：`AgentRequest`/`AgentResponse`/`EventDto`
  **以及那张能力表**从 client 移到 proto，让连接的两端用同一份定义；
  新增请求类型若不登记能力会被测试拦下。
- `link.rs` 的健壮性：单帧上限、超大帧拒绝（不按声明长度分配内存）、
  非法 JSON 回结构化错误、空闲超时（对端消失不会让服务端线程永久阻塞）、
  50 行一条注释说明"`from` 只做向前过滤"。
- `crates/pinbridge-client/tests/agent_link_end_to_end.rs`：真 socket 握手、
  版本协商、**双端闸门可区分**（客户端拦下的请求根本不到服务端，服务端只收到 5 条）、
  事件过链路逐字段完好。
- `pb_clear_breakpoint` 追加导出（只有"下"没有"清"会让使用者无法收回断点）。

### 新增（M0.5：C++ 桥接层）
- `native/src/agent_bridge.cpp` → `pinbridge_bridge.dll`：导出 Hub 期望的 8 个
  tool 符号并转发给真实 agent DLL。**不需要 Intel Pin**，因此"Hub ↔ agent"
  这条链路从"只测过类型"变成了"真实跑过"。
- `native/tests/bridge_roundtrip_test.cpp`：CTest 用例，含正向（真实 agent、
  能力闸门、事件跨边界往返）与反向（agent 缺失必须明确失败）两类。
- `crates/pinbridge-sys/tests/bridge_end_to_end.rs`：Hub 侧加载器端到端
  （真实 `LoadLibraryA` → ABI 校验 → 能力下发 → 事件取回 → 错误文本）。
  `PINBRIDGE_REQUIRE_NATIVE=1` 时**不允许跳过**（CI 的 native job 会设它）。
- `scripts/build-native.ps1`：一条命令完成 VS/vcvars/Ninja/CMake 发现、
  配置、构建与 CTest；本地与 CI 走同一条路径。
- `native/README.md`：这一层的构建方式、测试矩阵与"踩过的坑"。
- CI 新增 `native` job（windows-latest）：构建 agent + native 层 + CTest +
  加载器端到端测试。
- `scripts/verify.ps1` 新增 native 步骤（装了 VS 就自动跑）。

### 新增（M1：Pin 探针层落地，第一次真正插装一个进程）
- Pin 3.31 kit 就位并记录（版本串、zip 的 sha256、**实测**目录结构）。
  `scripts/fetch-pin.ps1` 的检查路径按实测布局修正——早先按记忆写成
  `source/tools/Pin/pin.H`（那是源码树内部的路径），结果脚本永远报"缺 Pin"。
- `native/src/pinbridge_tool.cpp`：Pin tool 骨架 + **入口点探针**。
  它只做三件事（注册探针、把事实推给 agent、开始/结束录制），
  依赖链是"探针 → 桥接层 → agent"——**复用**已测过的转发层，不重写一套。
  链路实测：`pin.exe -t pinbridge.dll -- pb_sample_target.exe`
  → 入口点命中 → 事件穿过两个语言边界 → **目标进程内写出 `.pbt``** →
  我们自己的读取器解析通过（`ctest -R pin_tool_instruments_a_target`）。
- `native/tests/sample_target.cpp`：自写的最小靶子。测试靶子一律自写，
  不拿系统二进制或外部样本——否则"测试失败"会同时包含"我们的工具坏了"
  与"那个二进制有反制"，两者必须能分开归因。
- `crates/pinbridge-agent/src/script.rs`：脚本**策略与生命周期**（M1 的执行层
  还没做，但登记、白名单、状态机、预算记账都能测了）。静态扫描给出**行号与理由**；
  文档与代码里都写明**它是策略过滤器而不是沙箱**（构造性绕过是可能的）。
  顺带把"生命周期上还相关（`is_live`）"与"该不该收新事件（`receives_events`）"
  拆成两个判断：混用会让被替换掉的旧脚本继续按老逻辑打日志。
- **ADR 0010**：探针层用 Pin，但当作可替换件。含 AMD 上的实测结论
  （Ryzen 7 9700X 上链路通）与备选（DynamoRIO/QBDI/TinyInst）。
- `scripts/build-native.ps1 -WithPin`、`native/CMakeLists.txt` 的 Pin 目标
  （参数逐条对应 kit 的官方 makefile，并注明行号）。

### 修复
- **桥接层漏了两个 tool ABI 导出**：`pb_start_trace`/`pb_stop_trace` 头文件声明了、
  agent 实现了，桥接层没转发——于是"让目标自己录一份轨迹"在桥接拓扑下不可达，
  而失败信息只是"导出面不完整"，看不出缺的是哪两个。这是 Pin tool 第一次跑起来
  才暴露的（Hub 侧加载器同样还没解析这两个符号，已记入路线图）。
- 轨迹元数据里的键名 `abel_version` 是错别字（应为 `abi_version`）。它不会被任何
  东西立刻报错，只会让读取方永远拿不到 ABI 版本；加了回归测试防止复发。
- `pinbridge-agent` 的桩实现状态码与错误文本不一致：内部先用 `Error::Trace` 走通用
  映射、再把码重映射成 `not_supported`，于是 `pb_last_error()` 里留着
  "内部错误: 轨迹错误: …"。现在 `stub()` 直接以 `NotSupported` 写文本，
  并删掉了 `mapped()` 这个重映射补丁。能力不足的文本也不再出现
  "能力不足: 能力不足:" 这种重复前缀。
- 链接路测试的偶发失败（"推 3 条取回 8 条"）：测试夹具的队列清理只排一次 64 条，
  上一个用例留下的多余事件会混进断言。现在循环拉空到 0，
  并把这条坑写进了夹具的注释（复位必须"干净"，不是"差不多干净"）。
- `pinbridge-sys` 在能力不足时把 `needed` 固定填成 `MemRead`：AI 会读到
  "需要 mem_read"并去申请一个它其实已经有的能力。`to_error`/`check` 现在
  要求调用方传入真实需要的能力；`Api` 记住下发过的能力集，`held` 如实回报。
  （`Api` 因此不再是 `Copy`——它持有状态，静默复制会让两份记录分叉。）
- `Status::NotSupported` 之前映射成 `Error::Trace`，会把读日志的人引向轨迹文件；
  现在映射为 `TargetUnavailable` 并带上"哪一层没实现"的说明。
- C++ 层首次真实编译暴露的三个缺陷：头文件缺 `<stddef.h>`（`offsetof` 找不到）、
  MSVC 未加 `/utf-8`（UTF-8 中文源码被按 GBK 解释，报出"字符串字面量中的换行符"
  这种与根因无关的错误）、`add_test` 用裸目标名导致 ctest 找不到可执行文件。
- MSVC 非英文界面下 `showIncludes` 前缀不匹配导致构建日志被刷屏（真正的错误被淹），
  现已显式设置 `CMAKE_CL_SHOWINCLUDES_PREFIX`。
- Python 侧首次通过 `ruff check` / `ruff format --check` / `mypy --strict`
  （本次 23 处 ruff 问题、5 处类型问题）：主要是行宽、`UP035` 导入来源、
  `UP037` 冗余引号，以及解码器参数类型（`bytes` → `bytes | bytearray`）。
  类型层面把"读取器到底依赖了什么"显式化成两方法的 `ReadableSource` Protocol，
  而不是假装满足 `BinaryIO`。
- `scripts/verify.ps1` / `verify.sh` 的 Python 侧从"只跑 pytest"补成
  pytest + ruff check + ruff format + mypy——这一步之前只在 CI 上跑，
  导致本地全绿而 CI 变红。
- `pinbridge-sys` 的 `PB_ABI_VERSION` 无条件导入：Windows 本地 clippy 全绿，
  Ubuntu 上因 `unused_imports` 直接失败。改为 `#[cfg(windows)]` 条件导入，
  测试模块显式导入；并把「跨平台 lint」加进 `scripts/verify.ps1` /
  `verify.sh` 与 `docs/dev/coding-standards-rust.md`。
- `commit-msg` 钩子的标题长度按字节计算导致合法中文标题被误判为超长；
  改为按 UTF-8 结构计算显示列（中文 2 列），不依赖 locale。

### 新增
- Rust 工作区（9 个 crate）：`core` / `proto` / `trace` / `hub` /
  `client` / `sys` / `agent` / `mcp` / `cli`，依赖方向单向。
- `.pbt` 轨迹格式 **v1**：chunk 容器 + delta/varint/zigzag 记录编码 +
  重复折叠标记；Rust 与 Python **双实现**共享同一组黄金字节用例。
- 重复折叠压缩：序号连续 + 载荷相同才折叠；压缩**可逆**（有测试）。
- 控制权模型：`Principal`（人与 AI 平级）、`CapabilitySet`（只减不增）、
  `ControlLease`（epoch 失效在途命令）。
- 会话状态机、有界事件通道（丢弃计数必须上报）、脚本生命周期与 sha256 溯源。
- MCP 服务：`initialize` / `tools/list` / `tools/call`，工具表按当前能力过滤，
  错误里带 `hint` 告诉 AI 下一步怎么办。
- CLI：`tools` / `replay` / `demo` / `mcp` / `version`，全部**离线可跑**。
- Python 侧：`.pbt` 解码、重放、结构化污点传播、OEP 候选定位、`pb-replay` 命令。
- 文档：架构/设计/规范/开发流程/ADR，共 20+ 篇，含对原文架构的逐条对照。
- 工程化：`rust-toolchain.toml` 钉版本、clippy pedantic 门禁、`deny.toml`
  供应链策略、CI（格式/lint/测试/端到端/Python/审计）、Issue 与 PR 模板。
- 端到端自检 `pinbridge demo`：建会话 → 拒绝越权 → 授权 → 装脚本 → 录轨迹 →
  折叠查询 → 读回校验 → 打印溯源。
- `NOTICE.md`：使用边界与第三方（Intel Pin）声明。`LICENSE` 保持纯 MIT 文本，
  便于 GitHub 自动识别许可证类型。

### 已知限制
- 所有需要 Pin 运行时支撑的动作是**桩实现**，显式返回 `not_supported`。
- agent 内嵌 CPython 尚未接通；`pb_load_script` 只做校验与报错。
- Hub 与 agent 之间的回环 TCP 链路已定义并实现传输抽象，真实握手见 M1。
- 桌面工作台（`apps/desktop`）只有骨架，未接真实数据。

### 不兼容变更
无（首个版本）。

<!--
提交 PR 时请顺手在 [Unreleased] 下加一条。CI 会检查含代码变更的 PR 是否
动过本文件（只动 docs/ 的 PR 可跳过）。
-->
