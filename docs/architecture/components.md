# 组件与职责

## 依赖方向（单向，不允许成环）

```text
                 pinbridge-core        （领域模型；零平台依赖、零 I/O）
                  ↑        ↑
     pinbridge-proto      pinbridge-trace
        ↑     ↑              ↑
        │   pinbridge-hub ────┘
        │      ↑
        ├─ pinbridge-client ──▶ pinbridge-sys ──▶ pinbridge-trace
        │                            ↑
        │                     pinbridge-agent（cdylib）
        └─ pinbridge-mcp ──▶ pinbridge-hub
           pinbridge-cli ──▶ pinbridge-hub / mcp / proto / trace
```

规则：**下层不知道上层存在**。`core` 里不允许出现"hub"、"mcp"这类词。
违反这条会让 agent（跑在目标进程里、不能带控制面代码）无法复用领域模型。

## 逐个组件

### `pinbridge-core` —— 领域模型

* 会话（`Session` + 状态机）、主体（`Principal`）、能力（`Capability`/`CapabilitySet`）、
  控制权（`ControlLease` + epoch）、有界通道（`bounded`，强制丢弃计数）。
* `#![forbid(unsafe_code)]`、无 I/O、无网络。
* 单测覆盖：状态机非法迁移、能力只减不增、epoch 拒绝旧命令、通道背压。

**改动提示**：新增能力位必须同步 `name()`（IPC 里的短名）、
`docs/specs/ipc-protocol.md` 的授权矩阵、以及 `native/include/pinbridge.h` 的位定义。

### `pinbridge-proto` —— 线路协议

* `frames.rs`：`Request` / `Response` / `ControlCommand`，以及面向 AI 的事件视图。
* `agent_link.rs`：Hub ↔ agent 的请求/响应类型**与那张能力表**
  （`AgentRequest::required_capability`）。放在这里而不是 client 里，是因为
  连接的两端（Hub 与目标侧 agent）必须用同一份定义——两侧各写一份，
  "同一条命令需要什么权限"迟早会分叉。
* `codec.rs`：行分隔 JSON，单帧上限 4 MiB（防一条坏包打满内存）。
* `mcp.rs`：MCP 工具描述表（名字、说明、JSON Schema、所需能力、是否在目标内执行）。

**改动提示**：字段删除/改名要提升 `WIRE_VERSION`；新增可选字段不需要。
新增请求类型要同步 `required_capability` 与 `docs/specs/ipc-protocol.md`。

### `pinbridge-trace` —— 事件与轨迹

* `event.rs`：`Event`（定长 8 参数）、`EventKind` 判别值表。
* `fold.rs`：重复折叠（序号连续 + 载荷相同）与**可逆**展开，含严格校验版本。
* `pbt.rs`：`.pbt` 流式编解码（chunk 容器 + delta/varint/zigzag）。
* `varint.rs`：LEB128 与 zigzag，区分"数据不足"与"文件损坏"。

**改动提示**：格式变必须提升 `FORMAT_VERSION`，并同步 Python 侧与黄金字节用例。

### `pinbridge-hub` —— 控制面

* `hub.rs`：`HubService`——所有写操作过"能力 → 控制权 → 会话状态"三关，
  成功与失败都写溯源。
* `runtime.rs`：事件通道 + 记录窗口 + 录制器接线（三个载体各自上报丢弃量）。
* `script.rs`：脚本注册表（同名校验、sha256 溯源、状态机）。
* `provenance.rs`：只追加的审计日志。
* `recorder.rs`：把 Hub 侧事件流落到 `.pbt`（内部做折叠）。

### `pinbridge-client` —— Hub → agent

* `Transport` 抽象（`MemoryTransport` 用于测试、`TcpTransport` 用于真链路）。
* `AgentClient`：请求/响应类型来自 `pinbridge-proto::agent_link`（见上），
  本 crate 只加"超时、能力快照、错误翻译"这三件客户端该管的事。
* **客户端侧也校验能力**：agent 侧还会再校验一次（双端校验）。
  客户端侧那一关的意义是"报文不出门"，目标侧那一关才是真正的边界。

**安全约束**：`TcpTransport::connect` 拒绝非回环地址。这不是可配置项。

### `pinbridge-sys` —— C ABI 类型层 + 加载器

* `abi.rs`：`PbEventRaw`（88 字节显式布局）、`PbSet`、`Status`/`PbStatus`。
* `loader.rs`：运行时 `LoadLibrary` + **先校验 ABI 版本再解析符号**。
* 允许 `unsafe`（理由写在 crate 文档里），是全仓库仅有的两处之一。

### `pinbridge-agent` —— 目标进程内

* `host.rs`：全局状态（一把 `Mutex`）、能力校验、事件采集热路径、本地录制器。
* `exports.rs`：C ABI 导出面。**桩函数先校验能力再返回 `not_supported`**——
  顺序不能反，否则权限链路在 M0 无法被测试覆盖。状态码与错误文本也必须一致
  （早期写法是"先报 Internal 再重映射成 not_supported"，于是文本里留着
  "内部错误: 轨迹错误"，读日志的人会以为发现了缺陷，而不是"这功能还没做"）。
* `link.rs`：**回环链路的服务端**。它不自己判断权限，而是转发给
  `exports::pb_*`——两条入口（C ABI 与回环链路）因此不可能对"该不该放行"产生分歧。
  规范见 [docs/specs/ipc-protocol.md](../specs/ipc-protocol.md) 的"agent 链路"一节。
* 允许 `unsafe`（同 `sys`）。

### `pinbridge-mcp` —— MCP 工具层

* JSON-RPC 2.0 over stdio；`initialize` / `tools/list` / `tools/call` / `shutdown`。
* `tools/list` **只返回当前能力允许的工具**：让 AI 看不到它做不到的事。
* 维护 epoch 缓存（人类接管后 AI 的在途命令会显式失败并给出下一步建议）。

### `pinbridge-cli` —— 命令行

* `tools` 打印工具表；`replay` 读轨迹；`demo` 端到端自检；`mcp` 起服务。
* 全部离线可跑，是"骨架有没有被改坏"的最快体检。

### `native/` —— C++ / Pin 层

* `include/pinbridge.h`：稳定 C ABI（与 `abi.rs` 逐字段对应）。
* `src/abi_selfcheck.cpp`：**不需要 Pin** 的布局自检（`static_assert`，编译期生效）。
* `src/agent_bridge.cpp`：**不需要 Pin** 的桥接 DLL（`pinbridge_bridge.dll`）——
  导出 Hub 期望的 8 个 tool 符号，转发给真实 agent DLL。
  这是"Hub ↔ agent"链路在没有 Pin 时也能被真实驱动的原因，也是 M1 里
  Pin tool 要复用的同一套转发拓扑。
* `src/pinbridge_tool.cpp` + `flags_virtualize.cpp`：真正的 Pin tool（M1，需 Pin SDK）。
* 构建与踩坑记录：[native/README.md](../../native/README.md)；
  目录内 `CMakeLists.txt` 在 `PINBRIDGE_WITH_PIN=OFF`（默认）下只构建桥接层与自检。

### `python/` —— 离线分析

* `pb/trace/pbt.py`：`.pbt` 的第二个实现（与 Rust 共享黄金字节用例）。
* `pb/trace/replay.py`：结构化污点传播、OEP 候选、按地址切片。
* `pb/trace/cli.py`：`pb-replay`。

### `apps/desktop/` —— 人的位置

* Tauri + React。人的角色是**观察员 + 接管者**：看事件流、看溯源、
  看 AI 正在跑哪一版脚本，必要时 takeover。

## 新增代码放哪：一张决策表

| 你写的东西 | 放这里 |
|-----------|--------|
| 纯类型/规则（无 I/O） | `pinbridge-core` |
| 会被外部（AI/CLI/工作台）看到的数据结构 | `pinbridge-proto` |
| 事件、轨迹、压缩 | `pinbridge-trace` |
| 会话编排、权限判断、脚本生命周期 | `pinbridge-hub` |
| 与目标进程说话 | `pinbridge-proto`（协议类型）/ `pinbridge-client`（客户端）/ `pinbridge-sys`（C ABI） |
| 跑在目标进程里的逻辑 | `pinbridge-agent` |
| 暴露给 AI 的动作 | `pinbridge-mcp`（工具表要同步 `docs/specs/mcp-tools.md`） |
| 人用的命令行 | `pinbridge-cli` |
| 离线分析算法 | `python/src/pb/` |
| 界面 | `apps/desktop/` |
