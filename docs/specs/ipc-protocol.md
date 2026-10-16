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
