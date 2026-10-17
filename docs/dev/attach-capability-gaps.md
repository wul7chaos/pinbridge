# 附加能力现状与缺口（2026-10-17 实测记录）

> 起因：需要用 pinbridge 对一个**已经在运行**的目标做动态分析（目标不能停、不能冻结）。
> 这份文档只记两件事：**现在能不能附加**、**不能的话差在哪**。
> 所有结论都在本机跑出来过，命令可直接复现（第 6 节）。

## 0. 一句话结论

**pinbridge 今天没有可用的"附加到已运行进程"。**

* 对外 API（MCP `pb_session_attach` / `HubService::attach` / C ABI `pb_attach`）都在，
  但 `HubService::attach` 只改会话字段（pid / 命令行 / 状态 / 溯源行），**不注入、不连接**；
* 免 Pin 的通路（`pinbridge_bridge.dll` → agent）设计上就是给"已运行进程"用的，
  但仓库里**没有注入器**；
* 目标侧的回环链路服务端（`LinkServer`）与 Hub 侧的 `AgentClient` **只有测试在用**，
  没有任何生产代码启动它或连它；
* Pin 自带的 `-pid` 附加在 **Windows 上直接断言失败，并且会把目标进程杀掉**（实测，见第 2 节）。

唯一现在能跑通的路径是第 3 节的**进程内注入**，已实测：**9 ms、目标不冻结、target 存活**。

## 1. 四个环节的现状

| 环节 | 现状 | 证据 |
|------|------|------|
| Pin `-pid` 附加 | ❌ Windows 上不可用，且**杀死目标** | `pin.log`：`attach_windows.cpp: ... ATTACH_FLOW_MANAGER_IMPL::StartSession: 517: assertion failed: m_pActiveSession == NULL`；pin.exe 退出码 `-1`；心跳靶子 2/2 都在附加瞬间停止写心跳 |
| Pin tool 的附加模式支持 | ❌ 工具恒调 `PIN_StartProgram()`，没有 `PIN_IsAttaching()` / `PIN_AddDetachFunction` / `PIN_Detach()` 分支 | `native/src/pinbridge_tool.cpp` 末尾（`PIN_StartProgram(); // 不返回`） |
| Hub 侧 `attach` | ⚠️ 只是**登记** | `crates/pinbridge-hub/src/hub.rs`：`attach()` 设置 `session.pid` / `command_line` / `transition(Attached)` / 写溯源，仅此 |
| agent 回环链路 | ⚠️ 实现且测过，**生产侧两头都没接** | `LinkServer::bind()` / `TcpTransport::connect()` 只出现在 `crates/pinbridge-client/tests/agent_link_end_to_end.rs` |
| 免 Pin 注入通路 | ⚠️ 材料齐全（桥接层会自己找同目录 agent），**缺一只注入器** | `agent_bridge.cpp` 的搜索顺序：`PINBRIDGE_AGENT_DLL` → 同目录 → 系统路径；`pb_start` 的报错文本自己写着"请改用 pb_attach 附加到已运行的进程" |

## 2. 实验一：`pin -pid`（Windows）——**会杀掉目标**

```powershell
# 靶子：一个每 20ms 往文件写时间戳的心跳进程（好用"写入间隔"量冻结时长）
pin.exe -pid <PID> -t native\build-pin\dist\pinbridge.dll `
        -pb_bridge <绝对路径> -pb_syscalls 0 -pb_mem 0 -pb_tf off
```

实测（两次，第二次用 `sudo` 提权，结论不变）：

```
pin.exe exited=True exitcode=-1
target alive after attach = False        # 心跳文件停在附加那一刻（40 → 42 行后不再增长）
pin.log: ...vm_w\attach_windows.cpp:517: assertion failed: m_pActiveSession == NULL
```

为什么这不是"没提权"：`sudo` 下 `elevated = True`，结果逐字相同。

**结论（这条最重要）**：**不要对在跑的游戏进程用 Pin 附加**——它会直接把进程带走。
要动态分析已经在跑的目标，只能走第 3 节的进程内注入，或让目标从一开始就在 Pin 下启动。

## 3. 实验二：免 Pin 的进程内注入——可行，**且不冻结目标**

原理（不需要改 pinbridge 的任何产品代码）：

```
OpenProcess → VirtualAllocEx + WriteProcessMemory(路径)
  → CreateRemoteThread(kernel32!LoadLibraryW, 路径)      # 桥接层进目标进程
      → 桥接层按同目录规则加载 pinbridge_agent.dll       # agent 也进目标进程
  → CreateRemoteThread(远端模块基址 + 导出 RVA, 参数)     # 直接调 C ABI
```

`int32_t` 返回值的导出（`pb_abi_version` / `pb_attach` / `pb_set_capabilities` …）
**返回值就是线程退出码**，不需要额外通道。

实测（靶子：心跳 pwsh；观察心跳文件的写入间隔）：

```
✓ 注入完成：LoadLibraryW 态耗时 9 ms，目标 存活
✓ pb_abi_version(0x1B0) → 0x1      # ABI v1
✓ pb_attach(0x1B0)      → 0x0      # PB_STATUS_OK（这条返回 0 说明 agent 已在目标内应答）
  目标进程最终状态 = 存活
  心跳最大间隔 47 ms（注入前基线也是 45–47 ms → 注入没有造成可测量的冻结）
```

脚本：[../scripts/inject-agent.ps1](../scripts/inject-agent.ps1)（本仓库，原型）。

### 3.2 应用实例：一个**正在跑的游戏进程**（实测，2026-10-17）

把同一脚本用在一个 5.6 GB、68 线程、已经玩了 8.5 分钟的在跑游戏进程上：
`pb_abi_version → 1`、`pb_attach(PID) → 0`，**注入耗时同样 9 ms**。

| 观测量 | 注入前 | 注入后 |
|--------|--------|--------|
| 线程数 | 68 | 68（不变） |
| 工作集 | 5623 MB | 5612 MB（不变） |
| 3 秒内 CPU 增长 | — | **+1.45 s**（说明进程仍在正常跑，没有被挂住） |
| 窗口响应 | Responding = True | Responding = True |
| 进程 | 存活 | 存活 |

也就是说：**"附加但不冻结"这件事在这台机器上是被验证过的**；
需要放弃的是 Pin 的 `-pid` 那条路，不是"附加"本身。

### 3.3 三个必须踩过的坑（都是实测撞的）

1. **`GetExitCodeThread` 只有 32 位**：远端 `LoadLibraryW` 返回的模块基址
   （`0x7FFCBF3F0000`）被截成 `0xBF3F0000`，拿它算 RVA 会得到错误的函数地址——
   远程线程跳过去就是 `0xC0000005`，**目标进程直接死**。正确做法是用
   `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE)` 拿完整 64 位基址。
2. **拿不准就不许开枪**：调用任何导出之前，先 `ReadProcessMemory` 把远端函数前 8 字节
   与本地读出来比对，不一致就**不调用**并说明理由。（第 1 条那个坑就是靠这个避免复发的。）
3. **PowerShell 的自动变量不能当参数名**：`param([int]$Pid)` 会直接报
   "无法覆盖变量 Pid，因为它是只读变量"，`$args` / `$home` 同理。

## 4. 缺口清单（按"要补什么"排序）

| 编号 | 缺口 | 影响 | 最小修法 |
|------|------|------|----------|
| G1 | Pin `-pid` 在 Windows 上断言并杀进程 | 无法用 Pin 附加任何已运行目标（含游戏） | 不走这条路：改用注入（G3）；或在别的 Pin 版本/平台重测后再定 |
| G2 | `HubService::attach` 只登记 | `pb_session_attach` 成功 ≠ 目标里有东西 | attach 时必须真的注入（或拒绝并说明"只登记"） |
| G3 | 没有注入器；agent 的 `LinkServer` 没有生产入口，Hub 也没有生产侧的 `AgentClient` | agent 装进去了也只能被"远程调导出"用，Hub 的工具（`pb_memory_read`/`pb_modules`/脚本）拿不到 | ① agent 增加 `pb_link_start(端口文件)` 导出（bind 后把端口写文件）；② MCP 侧 attach 后读端口文件并 `TcpTransport::connect` → `ClientLink` → `attach_link` |
| G4 | 工具恒调 `PIN_StartProgram()`，无附加/分离分支 | 即使 G1 修好，附加态也会走错路径；且无法优雅分离 | 用 `PIN_IsAttaching()` 分支 + `PIN_AddDetachFunction` + `PIN_Detach()` |
| G5 | ~~**`dist/` 不自包含**~~ **已修（见第 7 节）**：工具按名 `LoadLibraryA("pinbridge_bridge.dll")`，而 CMake 只把 *agent* 拷进 `dist/` | 目标不在构建目录时**必然加载失败**（实测）；`ctest` 之所以过，只因为靶子 `pb_sample_target.exe` 就在 `native/build-pin`（那儿正好有桥接层） | ~~`dist/` 里同时拷 `pinbridge_bridge.dll`~~ **已做**：`native/CMakeLists.txt` 增加一条 `copy_if_different $<TARGET_FILE:pinbridge_bridge>` |
| G6 | pin.exe 在工具启动失败时仍返回 **0** | 自动化只能靠解析文本判成败 | 需要一个可机读的失败信号（`-error_file` 或让 Pin 透传退出码） |
| G7 | 工具对**每个基本块**都要过 `INS_AddInstrumentFunction(OnInstruction)` | 长跑目标（游戏、浏览器）代价未知；附加场景下可能直接把人卡掉线 | 附加场景应有"按需插桩"档位（只挂 syscall/pattern，不注册全局 INS） |
| G8 | 断点/单步依赖"停下目标所有线程" | Pin 给不了（已知），对在线目标更不可接受 | 维持"不做调试器语义"的定位，别为它破例 |

### 4.1 G5 的可复现证据

```powershell
# 目标故意放在构建目录之外，CWD 设成 dist（也就是文档教的用法）
Copy-Item native\build-pin\pb_sample_target.exe $env:TEMP\pbt.exe
cd native\build-pin\dist
pin.exe -t pinbridge.dll -pb_syscalls 0 -pb_tf off -- $env:TEMP\pbt.exe
# → [pinbridge] 无法加载转发层 pinbridge_bridge.dll
#   → 请先 cd 到 dist 目录，或用 -pb_bridge <完整路径> 指定

# 加上绝对路径就正常（绕过方式，已验证）
pin.exe -t pinbridge.dll -pb_bridge <repo>\native\build-pin\pinbridge_bridge.dll ... -- $env:TEMP\pbt.exe
```

注意最后一行：**失败那次 `pin.exe` 的退出码也是 0**（见 G6）。

## 5. 本次刻意没做的事（边界）

* **没有对任何在线游戏进程做 Pin 附加**——实验一证明那会杀进程；
* 注入验证先在**一次性心跳进程**上完成两轮（脚本行为逐条确认），
  之后才对那个已在运行的游戏进程做了**同一条命令**的注入（结果见 3.2）。
  注入进去的 agent 目前是**惰性**的：除了 `pb_attach` 的状态标记，没有读/写目标内存，
  也没有布任何探针（链路缺口 G3 未修，Hub 还驱动不了它）；
* **没有改 Rust / C++ 产品代码**：本轮只加了这份文档与 `scripts/inject-agent.ps1` 原型；
* 没有碰断点/单步/内存写。

## 6. 复现命令（一条条都能跑）

```powershell
# ⑥ 心跳靶子：每 20ms 往文件写一行「Stopwatch 毫秒 + 墙钟」——用它量"目标被冻结了多久"
#    （相邻两行的最大差值 = 目标最久一次没有跑到自己的代码）
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($true) {
  [IO.File]::AppendAllText("hb.txt", "{0} {1}`r`n" -f $sw.ElapsedMilliseconds, (Get-Date -Format 'HH:mm:ss.fff'))
  Start-Sleep -Milliseconds 20
}
# 判据：注入/附加前后把 hb.txt 每行第一个数字取出来求相邻差；基线抖动约 45–55 ms，
#       注入成功也不会超过它；**目标被杀**的表现是文件从此不再增长。

# ① Pin 自带附加：观察"断言 + 目标死亡"
pin.exe -pid <心跳靶子 PID> -t native\build-pin\dist\pinbridge.dll `
        -pb_bridge <repo>\native\build-pin\pinbridge_bridge.dll -pb_syscalls 0 -pb_tf off
# → pin.exe exit=-1；pin.log 里 attach_windows.cpp:517 断言；hb.txt 停止增长

# ② 进程内注入：观察"9 ms、目标存活、导出返回"
pwsh -NoProfile -File scripts\inject-agent.ps1 -TargetPid <PID>

# ③ 启动模式的对照：目标在构建目录内时一切正常（ctest 过）
cd native\build-pin ; ctest -R pin_tool_instruments_a_target -V
```


## 7. 独立复现与已修项（2026-10-17 追加）

上面的结论我没有照单全收，而是**在自写心跳靶子上重跑了两条实验**（**全程没有对任何游戏进程做
Pin 附加**；第二条注入是在一次性 pwsh 心跳进程上做的）。

### 7.1 实验一复现：`pin -pid` 确实会杀进程

```
靶子 pid=15456  起始心跳行数=58
pin.exe 退出码=-1
靶子存活=False   结束时心跳行数=60        ← 只在附加瞬间多写了 2 行，随后不再增长
```

⇒ 与第 2 节一致 ✓。**结论不变：不要对在跑的目标用 Pin 附加。**

### 7.2 实验二复现：注入可用

```
本地 pinbridge_bridge.dll 基址 = 0x7FFCAD0D0000
✓ 已注入 pinbridge_bridge.dll：LoadLibrary 态耗时 8 ms，目标 存活
远端 pinbridge_bridge.dll 基址 = 0x7FFCAD0D0000
✓ pb_abi_version(PID) → 0x1  [rva 0x3610]
✓ pb_attach(PID)      → 0x0  [rva 0x3690]
目标进程最终状态 = 存活
靶子存活=True  心跳行数 57 → 78  相邻最大间隔 = 98 ms
```

⇒ 注入与导出调用**可复现** ✓。

**但有一点必须说清**：我这轮的"最大心跳间隔 98 ms"**比第 3 节的 47 ms 大**，而测量里混进了
pwsh 启动与同机并发（注入器自己也在这个 shell 里跑），所以**我这次的数据不足以证明"没有冻结"**。
诚实结论：这条实验证明了"**目标存活、心跳继续**"，**没有**证明"冻结时长 < 某阈值"。
要证明后者得用第 6 节那套判据，在更安静的机器上量基线。

### 7.3 已修：G5（`dist/` 不自包含）

`native/CMakeLists.txt` 里原来只有 agent 的拷贝，注释却写着"`cd dist && pin.exe …` 直接就能跑通"，
而工具在 CWD=dist 时按名去找 `pinbridge_bridge.dll` —— 目标不在构建目录就必然失败。
**用例一直绿只是因为靶子恰好躺在构建目录里**（即"测了一个用户不会遇到的位置"）。

修法：再加一条 `copy_if_different $<TARGET_FILE:pinbridge_bridge>` → `dist/pinbridge_bridge.dll`。

验证（就是第 4.1 节那个**必失败**的场景）：

```powershell
Copy-Item native\build-pin\pb_sample_target.exe $env:TEMP\pbt_outside.exe
cd native\build-pin\dist
pin.exe -t pinbridge.dll -pb_syscalls 0 -pb_tf off -- $env:TEMP\pbt_outside.exe --spin 20
# 修前：[pinbridge] 无法加载转发层 pinbridge_bridge.dll
# 修后：[pinbridge] 转发层就绪：pinbridge_bridge.dll（ABI 1） … pb_sample_target: 42
```

### 7.4 注入器加固：等待结果必须检查

`CreateRemoteThread` + `WaitForSingleObject` 的返回值以前没看。超时时
`GetExitCodeThread` 会给 `STILL_ACTIVE = 259`，既不是 0 也不是模块基址 ——
以前会拿一个"还没加载完"的状态当成功，随后算出的 RVA 指向错误地址。
现在两处（`LoadLibraryW` 与每个导出调用）都先检查等待结果，超时就不继续并说明理由。

### 7.5 仍然缺的（下一步）

| 编号 | 状态 |
|------|------|
| G1 Pin `-pid` | 未修（换注入这条路，见 7.1/7.2） |
| G2 `attach` 只登记 | 未修 —— 现在**至少能说清**"只登记"，但要么真注入、要么拒绝并说明 |
| G3 agent 链路进 Hub | **未修，这是让它真正有用的那一步**：① agent 增加 `pb_link_start(端口文件)` 导出；② MCP 侧 attach 后读端口文件并 `TcpTransport::connect` → `ClientLink` → `attach_link` |
| G4 附加/分离分支 | 未修（`PIN_IsAttaching`/`PIN_Detach` 命中数仍为 0） |
| G6 pin.exe 失败仍返回 0 | 未修（只能靠解析文本判成败，已在本文档记录） |
| G7 全量 INS 插装代价 | 未测（长跑目标上的代价未知） |
| G8 断点依赖停线程 | 维持"不做调试器语义"的定位 |

## 8. G2 + G3 已修：附加之后**真的能分析**（2026-10-17）

第 7 节的结论是"注入能通，但 Hub 够不到 agent"。这一节把它接上了。

### 8.1 agent 侧新增导出 `pb_link_start(port_file)`

以前 `LinkServer` **只有测试在启动**（生产侧没有入口）。现在是 agent 的第 40 个导出：

* 绑定回环端口 → 起一个**一直服务到进程退出**的线程 → 把 `127.0.0.1:PORT` 写进你给的文件；
* **幂等**：重复调用返回同一个端口（否则每次附加都换端口，调用方没法稳定连上）；
* 失败返回 `PB_STATUS_INTERNAL`，文案进 `pb_last_error`（不静默）。

用例：`crates/pinbridge-client/tests/agent_link_start_export.rs` ——
**真的连上去握手**（`pinbridge-agent 0.1.0 / x86_64`），而不只是断言"返回 0"
（那是"桩函数假装成功"的经典假通过）。

### 8.2 MCP 侧：`pb_session_attach` 会说真话（G2）并真的连（G3）

`pb_session_attach` 现在做两件事：先登记（原样），再按 `PINBRIDGE_AGENT_PORT_FILE`
指向的文件去连 agent；连上就 `attach_link`。回话分三种，**都能被用例断言**：

```
① 没有端口文件：已附加到 pid=4242（会话 1，epoch=1）；**仅登记**：没有配置 agent 端口文件（设 PINBRIDGE_AGENT_PORT_FILE …）
② 端口文件有效：已附加到 pid=4242（…）；链路已连上 127.0.0.1:60087（agent 在自己进程里应答）（agent 链路（tcp://…））
③ 连不上：      已附加到 pid=4242（…）；**仅登记**：连不上 127.0.0.1:60089（os error 10061）
```

以前**一律**回"已附加" —— 而目标里其实什么都没有。现在"仅登记"三个字是硬要求：
**只要没连上就必须出现**（用例 `attach_reports_honestly_whether_the_agent_link_is_live`）。

顺带一条：这些用例先要靠人类把 `process_control` 授给 AI 才能调 attach ——
这本身就是设计的体现（AI 默认只有只读基线 `[mem_read, trace_record]`，闸门先拦下来）。

### 8.3 注入器：一条命令把链路接起来

```powershell
pwsh -NoProfile -File scripts/inject-agent.ps1 -TargetPid <PID> `
     -Dll <repo>\native\build-pin\dist\pinbridge_bridge.dll `
     -LinkPortFile "$env:TEMP\pb-agent-port.txt"
# 然后让 MCP 指向同一个文件：
$env:PINBRIDGE_AGENT_PORT_FILE = "$env:TEMP\pb-agent-port.txt"
```

实测（自写心跳靶子，第三次尝试才通 —— 前两次的失败都是**真问题**）：

```
✓ 已注入 pinbridge_bridge.dll：LoadLibrary 态耗时 9 ms，目标 存活
✓ pb_abi_version(0x2570) → 0x1
✓ pb_attach(0x2570)      → 0x0
远端 pinbridge_agent.dll 基址 = 0x7FFC83B20000
✓ agent 链路已启动：127.0.0.1:64528（写入 …\pb-agent-port3.txt）
✓ 从外部真连上（TCP 已建立）   靶子存活=True
```

**前两次失败留下的教训（都写进脚本注释了）**：

1. `pb_link_start` 是 **agent** 的导出，**不是桥接层**的 —— 拿桥接层去 `GetExport` 会得到
   "Unable to find an entry point"，而桥接层只是"把 agent 带进目标"的那一段；
2. `dist/pinbridge_agent.dll` 是**旧的**（05:02 构建，那时还没有这个导出）——
   agent 是 Rust cdylib，必须 `cargo build -p pinbridge-agent --release` 后用新产物替换
   `dist/` 里那份；`build-native.ps1` 不会替你重编 Rust 侧。

### 8.4 还没验的那一步（说清楚）

**没有**做过"注入 → 设环境变量 → 起 `pinbridge-mcp` → `pb_session_attach` → 调一个真正
用到链路的工具"这条**一次性**的端到端。分开验过的是：agent 导出能连能握手（8.1）、
MCP 的接线与三种回话（8.2）、注入器能把端口文件写出来且端口真的可连（8.3）。
把这三段拼起来的那一次运行，是下一步最该做的事。
