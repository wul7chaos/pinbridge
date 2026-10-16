# native/ —— C++ / Pin 层

这一层是 C ABI 的 **C++ 侧**，产物由 Hub 在运行时加载。
规范见 [docs/specs/c-abi.md](../docs/specs/c-abi.md)，头文件是
[include/pinbridge.h](include/pinbridge.h)。

## 两个产物，同一套 ABI

| 产物 | 源文件 | 需要 Pin | 用途 |
|------|--------|----------|------|
| `pinbridge_bridge.dll` | `src/agent_bridge.cpp` | ❌ | 导出 Hub 期望的 8 个 tool 符号，转发给真实 agent DLL |
| `pinbridge.dll` | `src/pinbridge_tool.cpp`（M1） | ✅ | 同一套转发 + Pin 探针、JIT 上下文、TF 虚拟化 |

`src/abi_selfcheck.cpp` 与两个产物都有关：它用 `static_assert` 把跨语言结构体
布局、能力位、事件类型判别值钉在**编译期**。布局漂移应该是编译失败，
而不是运行期把目标进程搞崩。

## 构建

```powershell
pwsh scripts/build-native.ps1            # 配置 + 构建 + ctest
pwsh scripts/build-native.ps1 -RequireAgent   # agent DLL 缺失即失败（CI 用）
```

脚本会自动找 Visual Studio（`vswhere` → `vcvars64.bat`）、Ninja，
以及 CMake（系统没有时用 `uv tool install cmake` 拉一个）。
手工等价命令：

```powershell
cmake -S native -B native/build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build native/build
ctest --test-dir native/build --output-on-failure
```

## 测试

| CTest | 验证什么 |
|-------|----------|
| `abi_selfcheck` | C 头文件里的布局/位/判别值与预期一致（运行期复查编译期断言） |
| `bridge_roundtrip` | 真实 agent 在场：符号表完整、ABI 一致、**能力闸门跨语言生效**、桩明确返回不支持、3 条事件跨两个语言边界后逐字段完好、参数校验语义正确 |
| `bridge_missing_agent` | agent 缺失时**明确失败**（错误文本给出尝试过的路径），不静默成功 |

Hub 侧还有一条对应的 Rust 集成测试
（[`crates/pinbridge-sys/tests/bridge_end_to_end.rs`](../crates/pinbridge-sys/tests/bridge_end_to_end.rs)）：
它用**真实的 `LoadLibraryA`** 加载 `pinbridge_bridge.dll` 并驱动整条链。
CI 的 `native` job 会设置 `PINBRIDGE_REQUIRE_NATIVE=1`，因此它**不允许跳过**。

## 踩过的坑（别再踩）

| 现象 | 真正原因 | 处理 |
|------|----------|------|
| `error C2001: 字符串字面量中的换行符`，指向一堆无关的行 | 源文件是 UTF-8，MSVC 按系统代码页（936/GBK）解释，连续 UTF-8 字节吞掉了收尾引号 | 加 `/utf-8`（已在 CMakeLists 里对所有 MSVC 构建生效） |
| `ctest` 报 `Unable to find executable: .../Development/x.exe.exe` | `add_test(COMMAND <裸目标名>)` 在部分生成器下解析错误 | 用 `$<TARGET_FILE:...>` |
| 构建日志被上千行"注意: 包含文件:"刷屏，真正的错误淹没其中 | 非英文 MSVC 的 `showIncludes` 前缀与 Ninja 的默认解析不符 | 设 `CMAKE_CL_SHOWINCLUDES_PREFIX` |
| `offsetof` 在头文件里"找不到标识符" | 头文件只包含了 `<stdint.h>` | 补 `<stddef.h>` |

## Intel Pin（M1）

Pin 按 Intel 自己的许可分发，**不入库**（见
[third_party/pin/README.md](../third_party/pin/README.md)）。
启用方式：

```powershell
cmake -S native -B native/build -G Ninja -DPINBRIDGE_WITH_PIN=ON `
      -DPIN_ROOT="<pin>/source/tools/Pin"
```

在此之前，`pinbridge.dll`、`pinbridge_tool.cpp`、`flags_virtualize.cpp`
都不参与构建；需要它们的动作在桥接层与 agent 里都是**显式的 `not_supported`**。

## Pin tool（需要 Intel Pin）

`native/src/pinbridge_tool.cpp` 是探针层。它**只**做三件事：注册探针、把观察到
的事实推给 agent、开始/结束录制——权限判断与文件格式都在 Rust 侧
（理由见 [ADR 0010](../docs/adr/0010-pin-now-dynamorio-as-swappable-probe-layer.md)）。
因此它的依赖链是"复用"而不是"重写"：

```text
pin.exe
  └─ pinbridge.dll（探针：目前是入口点探针）
       └─ pinbridge_bridge.dll（转发层，已由 CTest 覆盖）
            └─ pinbridge_agent.dll（Rust：事件、脚本宿主、录制）
```

### 构建与运行

```powershell
pwsh scripts/build-native.ps1 -WithPin -RequireAgent     # 需要 third_party/pin/ 就位
cd native/build/dist                                     # dist 是自包含目录
..\..\..\third_party\pin\pin.exe -t pinbridge.dll -pb_trace out.pbt -pb_events 16 -- ..\pb_sample_target.exe
```

旋钮：`-pb_trace <path>`、`-pb_bridge <path>`（默认按名字找同目录）、
`-pb_caps all|ro`（默认 ro＝只读基线 257）、`-pb_events <n>`。

### 已知缺陷：系统调用探针在高频路径上不可靠（默认关闭）

`-pb_syscalls 1` 打开后会崩，Pin 报：

```
E: UPC Dispatcher: Unhandled internal exception in Pin or tool.
   Exception Code: RECEIVED_ACCESS_FAULT. Access Address = 0x000000008.
```

故障 IP 落在 `pinbridge_bridge.dll` 映像内、导出表**之前**（即静态 CRT 区域），
栈上没有任何线索。

**定位过程与结论（可复现）**：

| 实验 | 结果 |
|------|------|
| 回调里**零外部调用**（只计数、不入队） | 3/3 正常，跑完全程（捕获 enter=448） |
| 恢复在回调里调用桥接层 | 2/2 崩溃 |
| 只去掉 push_event、保留 allocate_sequence | 仍崩 |
| 排除自身模块（桥接层/agent 不插桩） | 仍崩（**这是必须做的正确性修复，但不是本崩溃的原因**） |
| 钉住模块（GET_MODULE_HANDLE_EX_FLAG_PIN） | 仍崩 |
| 错误缓冲从 thread_local 改为普通静态量 | 仍崩（同样必须做：静态 CRT 的 TLS 在 DLL 卸载后不可用） |

所以根因是**"在系统调用回调上下文里调用外部 DLL"本身**，与事件量只是弱相关
（`--syscalls 30` 也复现过）。因此：

* **`-pb_syscalls` 默认为 0**：把一个已知会崩的功能默认打开，不叫"可用"；
* 入口点探针不受影响（一辈子只命中一次，且在程序早期）；
* 正确修法已经明确：**回调只做内存入队**（一次比较 + 一次拷贝，不碰外部代码），
  由 `PIN_SpawnInternalThread` 起的内部线程跨边界推事件——内部线程不是回调
  上下文，也不被插桩。内存访问/模板匹配这两类高频探针都必须走同一条路。

另外：`INS_IsSysret` 在当前 Windows 目标上**没有匹配到**（收尾统计里 `exit=0`），
退出事件要另找机制（`sysret` 只是返回路径之一）。

### 验收


```powershell
ctest --test-dir native/build -R pin_tool_instruments_a_target --output-on-failure
```

判据不是"退出码为 0"，而是 stderr 里**必须**出现"入口点命中"——一份"安静成功"
的 Pin tool 无法区分"探针没命中"与"事件通道断了"。

### 踩过的坑（都写在 `third_party/pin/README.md` 与 CMake 注释里）

* Pin 的 MSVC 参数以 `source/tools/Config/makefile.win.config` 为**唯一权威**；
  链接需要 `/EXPORT:main /ENTRY:Ptrace_DllMainCRTStartup`，且 **`/MD`**（不是 `/MT`）；
* `pincrt.lib` 在 `intel64/runtime/pincrt/`，XED 头在 `extras/xed-intel64/include/xed`；
* `_WINDOWS_H_PATH_` 必须是**不带引号**的裸路径（Pin 的 `windows.h` 会拼成
  `<.../Windows.h>`）；
* Pin 自带部分重实现的 `minwindef.h`（含 `typedef int BOOL`），与真实 `windows.h`
  同处一个编译单元会报"BOOL 不明确的符号"——所以探针层只用三个 Win32 入口并按
  名字自行声明，不包含 `windows.h`；
* Pin 的包含顺序会引入它自带的 STL，`std::vector::data()`、`std::uint64_t`
  在这个编译单元里可能解析不到——用 Pin 自己的 `UINT64`/`UINT32`。