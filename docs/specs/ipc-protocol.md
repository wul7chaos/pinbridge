# Hub 线路协议 v1

适用链路：**HubService ↔ CLI / 工作台 / MCP 服务**（都是本机进程）。
Hub ↔ agent 是另一条更紧的链路，见 [c-abi.md](c-abi.md) 与
`crates/pinbridge-client/src/client.rs`。

## 传输

行分隔 JSON（NDJSON），一行一个对象，`\n` 结尾。

为什么不用长度前缀：本机回环链路上，用 `nc` / `socat` / 一行 Python
就能直接对话排查，这比"省几个字节"值钱得多。

**单帧上限 4 MiB**（`MAX_FRAME_BYTES`）。超过即视为协议错误并断开。
理由：最大的正常帧是 `ReadMemory`（默认 64 KiB → hex 后 ~88 KiB）与
`ExportTrace` 的摘要；留 4 倍余量已经足够。

## 请求

```json
{"op": "control", "session": 1, "epoch": 3, "command": {"cmd": "step", "count": 4}}
```

* `op` 是标签，字段扁平。**刻意不用 `{"method":…, "params":{…}}` 两层嵌套**：
  AI 生成工具调用时字段越浅越不容易错，人类手打也不容易漏括号。
* 任何**写操作**都必须携带 `epoch`（控制权代数）。只读操作不需要。

| `op` | 必需字段 | 需要能力 | 说明 |
|------|----------|----------|------|
| `hello` | `wire_version` `principal_kind` `principal_name` | — | 握手；返回分配的 epoch 与能力集 |
| `list_sessions` | — | — | 列出会话摘要 |
| `create_session` | `image_path` `arch` `args?` | `process_control` | 启动目标 |
| `attach` | `pid` | `process_control` | 附加到已运行进程 |
| `detach` | `session` | — | 断开但保留会话记录 |
| `control` | `session` `epoch` `command` | 见下表 | 下控制命令 |
| `load_script` | `session` `epoch` `name` `source` `subscribe?` | `script_load` | 注入脚本 |
| `unload_script` | `session` `epoch` `script` | `script_load` | 卸载脚本 |
| `read_memory` | `session` `address` `length` | `mem_read` | 读内存（hex 返回） |
| `write_memory` | `session` `epoch` `address` `hex_bytes` | `mem_write` | 写内存 |
| `read_registers` | `session` `thread` | `mem_read` | 读寄存器 |
| `grant_capabilities` | `session` `epoch` `target_principal` `capabilities` | `capability_grant`（**仅人类**） | 授权 |
| `start_trace` | `session` `epoch` `path` `profile?` | `trace_record` | 开始录制 |
| `stop_trace` | `session` `epoch` | `trace_record` | 停止录制，返回 summary |
| `query_events` | `session` `from` `limit?` `fold_repeats?` | `mem_read` | 拉事件批 |
| `provenance` | `session` | — | 读审计链 |
| `shutdown` | — | — | 优雅关闭 |

### 控制命令与所需能力

| `cmd` | 字段 | 需要能力 |
|-------|------|----------|
| `continue` | — | `process_control` |
| `pause` | — | `process_control` |
| `step` | `count?`（默认 1） | `single_step` |
| `run_to` | `address` | `single_step` |
| `breakpoint_set` | `address` `label?` | `breakpoint` |
| `breakpoint_clear` | `address` | `breakpoint` |
| `pattern_hook` | `pattern` `label?` | `pattern_hook` |

## 响应

```json
{"id": 7, "kind": "accepted", "epoch": 3}
```

* `id` 与请求对应（并发对账用）；
* `kind` 是标签；`hello` 响应额外带 `principal` 与 `capabilities`；
* 能力集的 JSON 形态是 `{"bits": 4095, "names": ["mem_read", …]}`——
  位图让程序可靠解析，名字让 AI 与人类一眼看出权限范围。

| `kind` | 载荷 |
|--------|------|
| `hello` | `wire_version` `epoch` `principal` `capabilities` |
| `sessions` | `items: SessionSummary[]` |
| `session` | `summary` |
| `accepted` | `epoch` |
| `script_loaded` | `script` `source_sha256` |
| `memory` | `address` `hex_bytes` |
| `registers` | `thread` `values: [[name, value]]` |
| `events` | `items: EventView[]` `next` `dropped` |
| `provenance` | `entries: string[]`（JSON Lines） |
| `ok` | — |

错误响应：

```json
{"id": 7, "error": {"code": "capability_denied", "message": "...", "hint": "让人类用 pb_capability_grant 授权"}}
```

`hint` 是**给 AI 的下一步**，不是给人看的排查建议。这是本协议里最容易被
忽略但最有价值的一个字段：AI 拿到可执行建议就不会开始瞎试。

### 错误码

| `code` | 触发 | 对应 Rust 错误 |
|--------|------|----------------|
| `target_unavailable` | 会话不存在 / 状态不允许 | `Error::TargetUnavailable` |
| `capability_denied` | 能力不足 | `Error::CapabilityDenied` |
| `control_conflict` | epoch 不匹配 | `Error::ControlConflict` |
| `script_error` | 脚本名/正文非法、状态不允许、已卸载 | `Error::Script` |
| `trace_error` | 轨迹读写失败 | `Error::Trace` |
| `protocol_error` | 编解码失败 | `Error::Protocol` |
| `io_error` | 底层 I/O | `Error::Io` |

## EventView（AI 直接消费的形态）

```json
{"seq": 8, "kind": "repeat", "thread": 100, "address": 8796139249664, "args": [3999, 3], "repeats": 3999}
```

* `repeats` 在非折叠事件上等于 1，并且**会被省略**（减少噪音）；
* `args` 会**去掉末尾的零**（`argc` 语义在 JSON 层的对应物）——
  定长 8 项在二进制里合理，在 JSON 里就是纯浪费；
* `dropped` 字段出现在响应里而不是只写日志：**必须让调用方看到**。

## 版本策略

* 删除/重命名字段、改变语义 → 提升 `WIRE_VERSION`（`pinbridge_core::WIRE_VERSION`）
  + 更新本文件 + `CHANGELOG.md`；
* 新增可选字段 → 不需要提升；
* 未知 `op` → 报 `protocol_error`（**不忽略**：静默忽略会让一次写操作
  看起来成功了）。

## 手搓一条请求（排查用）

```bash
# Windows（PowerShell）
'{"op":"list_sessions"}' | pinbridge-mcp --stdio-debug   # M1 提供的调试入口

# 或者直接用 python 对话（M1 起 Hub 支持独立监听）
python -c "
import json,socket
s=socket.create_connection(('127.0.0.1',PORT))
s.sendall((json.dumps({'op':'list_sessions'})+'\n').encode())
print(s.makefile().readline())
"
```

---

# agent 链路（Hub ↔ 目标进程内的 agent）

上面那套 NDJSON 是 **Hub 以上**（人/AI 与 Hub 之间）的协议。
Hub 与 **agent** 之间是另一条链路，帧格式与错误码都不同，规范如下。

## 两种链路拓扑（都在设计中，用途不同）

```text
A. 进程内（同机、Hub 能 LoadLibrary）
   Hub ──pinbridge-client──▶ pinbridge_bridge.dll / pinbridge.dll ──C ABI──▶ agent
   适用：Hub 自己用 `pin -t` 拉起目标；或 M0.5 的桥接层（无 Pin 时也能跑通整条链）
   规范：docs/specs/c-abi.md

B. 跨进程回环（agent 已注入，Hub 在别的进程里）
   Hub ──pinbridge-client::TcpTransport──▶ agent::LinkServer（127.0.0.1）──▶ exports::pb_*
   适用：附加到已经跑起来的目标
   规范：本节
```

两条链路**共用同一套闸门**：B 的 `LinkServer` 不自己判断权限，而是调用 A 用的那些
`pb_*` 导出。于是"同一条命令该不该放行"不可能在两条路径上出现分歧。

## 帧格式

`u32` 小端长度 + JSON 负载，**单帧上限 4 MiB**（与 Hub 侧共用同一个常量）。

```text
  ┌────────────┬──────────────────────────┐
  │ len (4 LE) │  JSON payload            │
  └────────────┴──────────────────────────┘
```

用 JSON 而不是二进制是**刻意的**：这条链路承载控制命令与**有上限的**事件批
（`drain_events.limit ≤ 4096`），而真正的高频数据路径是 agent 在目标进程内
直接把事件写进 `.pbt`。可读性的收益（`nc` 一下就能手工排查）大于编码开销。
需要传每秒十万级原始事件时（M2 的流式回传）应当**新增**一种二进制帧，
而不是把这个换成二进制——控制链路要保持可以手工对话。

**只允许回环**：目标侧的 `LinkServer::bind()` 没有地址参数，它只会绑定
`127.0.0.1`；Hub 侧的 `TcpTransport::connect` 拒绝非回环地址并给出明确错误。
这是硬约束，不是默认值。

## 请求（`req` 标签）

| `req` | 需要能力 | M0 状态 |
|-------|----------|---------|
| `ping` | `mem_read` | ✅ 含协议版本协商 |
| `read_memory` | `mem_read` | 桩（`not_supported`） |
| `write_memory` | `mem_write` | 桩 |
| `read_registers` | `mem_read` | 桩 |
| `set_breakpoint` / `clear_breakpoint` | `breakpoint` | 桩 |
| `single_step` / `run_to` | `single_step` | 桩 |
| `continue` / `pause` | `process_control` | 桩 |
| `load_script` / `unload_script` | `script_load` | 桩（M1 接 CPython） |
| `drain_events` | `mem_read` | ✅ |
| `start_trace` / `stop_trace` | `trace_record` | ✅ |

"需要能力"这一列**不是文档抄代码**，而是两端共用的那一张表
（`pinbridge_proto::agent_link::AgentRequest::required_capability`）：
客户端用它做发送前校验，agent 用它做收到后的校验。新增请求类型必须登记，
否则会落到兜底分支上——`every_request_maps_to_a_capability_and_a_name` 会拦下来。

## 响应（`res` 标签）

`pong` / `memory` / `registers` / `events` / `script` / `trace` / `trace_summary` /
`ok` / `error`。

两个容易踩的语义：

* `events.next` 是**下一批的游标**。没取到任何事件时它等于请求里的 `from`
  （不推进），这样"空批次"不会被误读成"已经消费掉一段"；
* `trace_summary.folded` 是 `events - records` 的**近似值**。精确的折叠条数只在
  `.pbt` 的 `SUMMARY` 段里——不假装精确是有意的。

### `from` 只做向前过滤

`drain_events.from` 只能过滤掉**比它更早**的事件；已经被取走的事件不会重现。
要完整历史必须读 `.pbt`（链路从来不是历史存储）。
这条语义在文档里写死，是因为它很容易被误解成"可以从任意序号回放"。

## 错误码

| `code` | 含义 | 使用者该做什么 |
|--------|------|----------------|
| `capability_denied` | 目标侧能力不足 | 去要权限（`pb_capability_grant`），不是重试 |
| `not_supported` | 该能力尚未实现/平台不具备 | 换方案；**不要**当异常重试 |
| `not_attached` | 尚未附加到目标（含 agent DLL 加载失败） | 先完成注入 |
| `invalid_argument` | 参数非法（空指针、hex 非法、长度越界……） | 修调用方 |
| `protocol_error` | 协议版本不匹配或帧非法 | 对齐版本；不要"能解析就继续" |
| `busy` / `timeout` / `internal` | 目标忙 / 超时 / 内部错误 | 按上下文处理 |

`capability_denied` 与 `not_supported` **必须可区分**：前者要人去开口子，
后者是"这功能还没做"。混成一个码会让 AI 在"申请权限"与"换方案"之间乱撞。

## 两次握手与重连

1. `ping` 携带 `wire_version`；不匹配时 agent 明确拒绝并给出双方版本；
2. 一次 TCP 连接承载**多次**往返（不是一条命令重连一次）；
3. 连接断开后，客户端重新 `connect` 并再次 `ping` 即可——agent 的
   `LinkServer::serve_connections(n)` 支持连续服务多个连接，
   而 agent 的状态（能力集、事件队列、录制器）**属于进程，不属于连接**，
   因此重连不会丢掉会话。

## 验证

| 用例 | 覆盖 |
|------|------|
| `link::tests::frames_round_trip_over_a_real_socket` | 真 socket 往返 + 非法 JSON 得到结构化错误 + 超大帧被拒 |
| `link::tests::ping_negotiates_the_protocol_version` | 版本协商 |
| `link::tests::the_two_gates_are_distinguishable_on_the_link` | 能力闸门与实现闸门可区分 |
| `link::tests::drain_events_filters_forward_and_trims_args` | 游标与 `args` 去零 |
| `client/tests/agent_link_end_to_end.rs` | 客户端 ↔ 真实链路 ↔ 真实 agent（含双端校验与重连可用性） |
| `link::tests::server_survives_a_closed_and_a_silent_peer` | 对端连上就断/不再发请求时，服务端不会永久阻塞 |

