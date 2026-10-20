# AI 会话实测问题清单（2026-10-20，PoE 附加场景）

> 场景：对**已在运行**的 `PathOfExile.exe`（PID 856，约 11 GB）做附加，
> 目标是"在 minimap 常量缓冲上传点断下来，取寄存器里的数据指针，再读出 4×4 变换矩阵"。
> 结论：**没能完成**，卡点全在框架侧，不在目标侧。下面每条都带复现步骤与证据原文。
>
> 环境：Windows / pinbridge `native\build-pin\dist` 现成产物 + `target\debug\pinbridge-mcp.exe`。
> 人类侧已通过 `pb_capability_grant` 授予全部能力
> （`mem_read|mem_write|reg_write|breakpoint|single_step|syscall_hook|pattern_hook|exception_steer|trace_record|process_control|script_load|capability_grant`，epoch=1），
> 所以下面所有失败都**不是能力不足**。

---

## P0-1 断点命中后，MCP 侧的链路被杀（os error 10053）

**严重度**：P0 —— 这一条直接废掉"断点 → 取数据"这个最核心的用法

### 现象

```
pb_breakpoint_set(address=140701500376192)      → 已在 0x7FF79EF30480 下断点
…（约 1 秒内命中，pb_session_list 显示 state=paused）
pb_memory_read(address=140701500376192, length=16)
  → Error: 目标不可用: 读内存请求失败：你的主机中的软件中止了一个已建立的连接。 (os error 10053)
```

此后**任何** `pb_memory_read` 都是同一个 10053，即使 `pb_control continue` 之后也一样。

### 复现步骤

1. `inject-agent.ps1 -TargetPid <pid> -Dll …\pinbridge_bridge.dll -LinkPortFile %TEMP%\pb-agent-port.txt`
2. `$env:PINBRIDGE_AGENT_PORT_FILE = "$env:TEMP\pb-agent-port.txt"`（MCP 侧）
3. `pb_session_attach(session=1, pid=<pid>)` → `链路已连上 127.0.0.1:58247`
4. `pb_memory_read(<任意已映射地址>, 16)` → **成功**（基线正常）
5. `pb_breakpoint_set` 打在一个**每帧都调用**的函数上（本次是 `PathOfExile.exe` RVA `0x1030480`）
6. 等 1 秒 → `pb_session_list` 显示 `paused`
7. `pb_memory_read(...)` → **10053**，且永久失效

### 判断

- 断点本身是**生效**的（state 确实转 paused，且 `pb_control continue` 后游戏恢复响应、CPU 时间继续增长，
  说明目标没有被冻死）；
- 失效的是**链路的服务端**：paused 之后 agent 的回环 listener 不再应答，
  对端等不到响应就 RST。10053 是"本机软件主动中止"，不是超时也不是拒绝连接。
- 也与"暂停是不是冻结全线程"有关：如果暂停把 agent 自己的服务线程一起挂住，
  那么 paused 期间**所有**请求必然失败。这与观察完全一致。

### 建议（按性价比排序）

1. **命中瞬间就地抓取**：探针命中时，把「寄存器快照 + 调用方指定的一段内存」**直接随事件回传**，
   让 AI 不需要在 paused 状态发起任何读请求。这是最稳的，也让 `pb_registers` 的语义自洽。
2. **paused 期间保证服务线程不被挂起**：Pin 侧挂起目标线程时，排除 agent 的 listener 线程；
   或者让 listener 走一个"常驻、不受暂停影响"的执行上下文。
3. **从探针侧缓存读**：`pb_memory_read` 在 paused 时改为从探针侧已缓存的内存快照读，
   并在返回里标注"数据时刻 = 命中时刻"。

---

## P0-2 链路断了之后没有任何重连路径（状态机硬死）

**严重度**：P0 —— 会话一旦链路断就报废，只能重启目标进程

### 现象

```
pb_session_attach(session=1, pid=856)   # 链路已死（10053）后想重连
  → Error: 目标不可用: 非法状态迁移 Running -> Attached（会话 1）

pb_control(pause)
pb_session_attach(session=1, pid=856)
  → Error: 目标不可用: 非法状态迁移 Paused -> Attached（会话 1）
```

另外：重新跑 `inject-agent.ps1` 能成功起一个**新**的 listener（端口仍是 58247，`pb_attach` 返回 0），
说明**目标侧是好的**，但 MCP 侧**没有任何入口把这个新链路接回会话**。

### 建议

- 增加 `pb_session_detach`（回到 Detached/Init），之后允许 `attach` 重连；
- 或让 `pb_session_attach` 在 `Attached/Paused/Running` 下**幂等**：检测链路死活，死了就重连；
- 或给 `pb_session_attach` 增加 `force: true`。

---

## P1-3 没有"清除断点"的接口

**严重度**：P1 —— 误设热函数断点后无法撤销

### 现象

- 工具面只有 `pb_breakpoint_set`，没有 `pb_breakpoint_clear` / `pb_breakpoint_list`；
- 本次我还曾把十进制地址算错，把断点下到了 `0x7FF7808EEE80`（一个**未映射**地址），
  框架返回 `已在 0x7FF7808EEE80 下断点`（**没有校验该地址是否落在可执行段**），
  之后也没有任何办法删掉它。

### 建议

- `pb_breakpoint_list` / `pb_breakpoint_clear(target=addr|all)`；
- `pb_breakpoint_set` 先校验：地址是否在某个 MEM_IMAGE 的可执行段内；不合法就**明确报错**而不是静默登记。

---

## P1-4 脚本顶层代码的 `pb.log` 不产生任何可见输出

**严重度**：P1 —— 脚本能力"能装但看不到任何东西"，等于不可用

### 现象

```python
pb_script_load(name="cb_dump", source=<含 pb.log(...) 与 pb.mark(...) 的顶层代码>)
  → 脚本 cb_dump 已加载（id=1，sha256=41beee66…）      # 成功

pb_events_query(from=11) → []   next=11 dropped=0      # 全空，连一条 log 事件都没有
```

`pb_control continue` 之后再查仍然为空。

### 可能的解释与建议

- 要么"顶层代码不在加载时执行，只在订阅事件被触发时执行"（但文档写的是加载即执行）；
- 要么 `pb.log` 的队列要靠 `_pb_drain_json` 在**下一次**某种时机才被抽走，本次没触发；
- 建议：
  1. 在 `pb_script_load` 的**返回体里直接带回脚本本次产生的 log 行**（AI 用起来最顺手）；
  2. 或提供显式 `pb.flush()`；
  3. 并在文档里写清"顶层是否立即执行、输出何时可见"。

---

## P2-5 `pb_session_attach` 必须先有 session，但"附加"没有对应的建会话入口

**严重度**：P2 —— 首次使用会卡一步

### 现象

```
pb_session_attach(pid=856, arch="x86_64")
  → Error: 缺少参数 session 或类型不是整数
```

`pb_session_create` 的语义是**启动目标**（`image_path` + `arch`），不是"附加到已运行进程"。
所以"目标已经在跑、不能重启"这条路上，第一步该调什么是不明确的。

### 建议

- `pb_session_attach` 允许不带 `session`：自动建一个"attach 型会话"并返回 id；
- 或在 `pb_session_create` 里区分 `mode: launch|attach`。

---

## P2-6 注入后的端口发现只能靠进程启动时的环境变量

**严重度**：P2

- `inject-agent.ps1 -LinkPortFile <path>` 会把端口写进文件，但 **MCP 侧没有任何工具能指定这个文件**，
  只能依赖 MCP 进程**启动时**的 `PINBRIDGE_AGENT_PORT_FILE`；
- 本次能连上，是因为宿主环境里恰好已经指向了同一个文件；如果文件路径变了，AI 无法自救。

### 建议

- `pb_session_attach(..., port_file=...)` / `pb_session_attach(..., port=58247)`；
- 或 `pb_session_set_link(path_or_port)`。

---

## P2-7 `pb_registers` 的可用前提没有在工具层表达

- 文档里的说法是"寄存器由探针命中时捕获，没命中过就没有快照"，但这个前提**只在报错文本里出现**；
- 建议 `pb_breakpoint_set` 的返回里就带上"命中后可用 `pb_registers(thread=<tid>)`"，
  或者干脆让事件里直接附 `thread_id` + 寄存器，省掉一次往返。

---

## 附：这次**工作正常**的部分（别改坏）

| 能力 | 实测 |
|---|---|
| 注入（免 Pin，附加到运行中进程） | ✅ `LoadLibrary` 9ms，目标存活，`pb_abi_version=1` |
| 环回链路 | ✅ `127.0.0.1:58247`，attach 回话明确区分"已连上 / 仅登记" |
| `pb_modules` | ✅ 列出 160+ 模块含基址与大小（走目标自己的加载器链） |
| `pb_memory_read` | ✅ 断点**之前**完全正常（本次用它跑了 5 GB 全内存扫描） |
| `pb_script_load` | ✅ 策略检查与装载都通过，返回 sha256 |
| `pb_provenance` | ✅ 审计链完整（谁在何时 attach / 授权 / 下断点 / continue / pause，含 epoch） |
| `pb_capability_grant` | ✅ 人工一次授权后 AI 侧能力齐备 |

**一句话总结**：注入、链路、读内存、审计这四层是好的；
**断点之后的一切（取寄存器、取内存、重连、撤断点、看脚本输出）是断的** ——
而"断点之后的一切"恰好是动态分析真正出活的部分。
