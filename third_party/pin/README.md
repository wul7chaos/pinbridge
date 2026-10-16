# Intel Pin（第三方）

PinBridge 选择插装框架作为基座（理由见
[docs/adr/0001-instrumentation-over-debug-api.md](../../docs/adr/0001-instrumentation-over-debug-api.md)），
Intel Pin 是其中的闭源代表。**Pin 按 Intel 自己的许可分发，不包含在本仓库里**，
`third_party/pin/` 目录在 `.gitignore` 中被排除（只保留本文件）。

## 为什么不入库

1. **许可**：Intel Pin 的许可不允许我们把它再分发到本仓库；
2. **体积**：解压后约 176 MB、两万余个文件，塞进 git 会让每次 clone 都变慢；
3. **可复现性**：我们在下面钉死版本与校验和，比把二进制塞进历史里更可审计。

## 已记录的版本

| 平台 | 包 | 版本串 | sha256（zip） | 大小 |
|------|----|--------|---------------|------|
| Windows x64 (msvc) | `pin-external-3.31-98869-gfa6f126a8-msvc-windows.zip` | `pin-3.31-98869-fa6f126a8` | `d6f492dcbb18ee5e8e0e23be4b07a8aac843e23c7d4bd2bc1283c91ac60f6632` | 40.1 MB |
| Linux x64 | （待填） | — | — | — |

验证方式（不需要 Pin 之外的东西）：

```powershell
Get-FileHash pin-external-3.31-98869-gfa6f126a8-msvc-windows.zip -Algorithm SHA256
third_party\pin\pin.exe -version     # 期望输出 Pin: pin-3.31-98869-fa6f126a8
```

## 真实目录结构（别再按记忆写路径）

解压后的布局与"凭印象猜"的结果并不一样——下面是**实测**的：

```text
third_party/pin/
  pin.exe                              ← 根驱动（按目标位数转发到 ia32/ 或 intel64/）
  intel64/bin/pin.exe                  ← 64 位真正的执行体
  intel64/lib/pin.lib                  ← 工具要链接的导入库
  intel64/lib-ext/ , intel64/runtime/pincrt/
  extras/xed-intel64/{include,lib}      ← XED（反汇编库）
  extras/crt/ , extras/stlport/ , extras/cxx/ , extras/components/
  source/include/pin/pin.H             ← 头文件在这里
  source/include/pin/gen/              ← 生成的头（prototype.h 等）
  source/tools/Include/pin.H           ← 工具传统的 PIN_ROOT 形态（与上面同一份）
  source/tools/Config/makefile.win.config  ← **构建参数的唯一权威**（见下）
  source/tools/ManualExamples/         ← 官方示例工具
  doc/html/                            ← 本地文档（离线也能查）
```

> 早期版本的 `scripts/fetch-pin.ps1` 检查的是 `source/tools/Pin/pin.H`
> （那是**源码树**内部的路径，kit 里不存在）。这种"文档凭记忆写"的漂移
> 代价很高：脚本会永远报"缺 Pin"，而实际装好了。现在脚本按上面这份实测布局检查。

## 构建工具 DLL

```powershell
pwsh scripts/build-native.ps1 -WithPin            # 找到 Pin 就带 -DPINBRIDGE_WITH_PIN=ON
# 或者手工：
cmake -S native -B native/build -G Ninja -DPINBRIDGE_WITH_PIN=ON `
      -DPIN_ROOT="$PWD/third_party/pin"
cmake --build native/build
```

产物 `native/build/dist/pinbridge.dll`。用法：

```powershell
third_party\pin\pin.exe -t native\build\dist\pinbridge.dll -pb_trace out.pbt -- TARGET.exe
```

### MSVC 下的编译/链接参数

不要凭经验猜这些开关：`source/tools/Config/makefile.win.config` 是唯一权威，
`native/CMakeLists.txt` 里的 Pin 目标逐条对应它（并注明了出处行号）。要点：

| 类别 | 关键项 | 为什么 |
|------|--------|--------|
| 头文件 | `source/include/pin`、`source/include/pin/gen`、`extras/xed-intel64/include`、`/FIinclude/msvc_compat.h` | 缺 `msvc_compat.h` 会报一堆莫名其妙的类型错误 |
| 预处理 | `PIN_CRT=1`、`TARGET_WINDOWS`、`TARGET_IA32E`、`HOST_IA32E`、`__LP64__` | Pin 用**自己的** CRT（`pincrt`） |
| 编译 | `/EHs- /EHa- /GR- /GS- /Oi- /MD /O2`、`/Zc:threadSafeInit- /Zc:sizedDealloc-` | 关掉 C++ 异常与 RTTI 是 Pin 工具的约定 |
| 链接 | `pin.lib xed.lib pinipc.lib pincrt.lib kernel32.lib` | 导入库在 `intel64/lib`、`intel64/lib-ext`、`extras/xed-intel64/lib` |
| 链接 | `/DLL /EXPORT:main /ENTRY:Ptrace_DllMainCRTStartup /NODEFAULTLIB` + `intel64/runtime/pincrt/crtbeginS.obj` | 工具的入口叫 **`main`**，且必须显式指定 CRT 启动对象 |

## AMD CPU 上能不能用（实测）

Intel 从未承诺在 AMD 上支持 Pin，社区里也有"SSE4.2 检测异常"之类的报告。
本仓库的开发机就是 AMD，所以这件事是**实测**过的，而不是照抄传闻：

| 检查 | 命令 | 结果（AMD Ryzen 7 9700X / Windows） |
|------|------|--------------------------------------|
| Pin 自身启动 | `pin.exe -version` | `Pin: pin-3.31-98869-fa6f126a8` |
| 不用工具拉目标 | `pin.exe -- cmd /c echo ok` | `ok`，退出码 0 |
| 真实插桩 | `pin.exe -t pinbridge.dll -- pb_sample_target.exe` | 入口点命中 → 事件穿过桥接层与 agent → `.pbt` 落盘 → 自己的读取器解析通过 |

结论与边界写在 [ADR 0010](../../docs/adr/0010-pin-now-dynamorio-as-swappable-probe-layer.md)：
**在这台 AMD 上链路是通的**，但"通"不等于"被支持"，所以探针层被刻意限制成
可替换件（只有 Pin tool 允许出现 `pin.H`）。真遇到 CPU 兼容问题时的备选是
DynamoRIO（BSD、官方支持 AMD64）、QBDI、TinyInst——换基座只需要重写探针层。

## 构建工具 DLL 时的踩坑清单（都验证过）

1. **参数以 kit 里的 `source/tools/Config/makefile.win.config` 为准**，别按经验猜；
2. 链接需要 `/EXPORT:main`（Pin 调用的入口叫 `main`）与
   `/ENTRY:Ptrace_DllMainCRTStartup`，以及 `intel64/runtime/pincrt/crtbeginS.obj`；
3. **必须 `/MD`**（与仓库其它目标的 `/MT` 相反），否则报 pincrt/libcmt 冲突；
4. `pincrt.lib` 在 `intel64/runtime/pincrt/`，**不在** `intel64/lib/`；
5. XED 头在 `extras/xed-intel64/include/xed`（末尾的 `xed` 不能少，
   少了会报"找不到 xed-interface.h"，看起来像缺文件）；
6. `_WINDOWS_H_PATH_` 必须是**不带引号**的裸路径：Pin 的 `extras/crt/include/windows.h`
   把它拼成 `<.../Windows.h>`，多一对引号就会得到 `<"C:/..."/Windows.h>` 这种怪路径；
7. Pin 自带部分重实现的 `minwindef.h`（`extras/crt/include/my_minwindef.h:39`
   有 `typedef int BOOL`），与真实 `windows.h` 在同一个编译单元里必然冲突
   （"BOOL：不明确的符号"，错误位置全在系统头里）。探针层因此**不包含
   `windows.h`**，只按名字声明它需要的三个 Win32 入口；
8. Pin 的包含顺序会引入它自带的 STL/CRT，实测 `std::vector::data()` 与
   `std::uint64_t` 在那个编译单元里解析不到——用 Pin 自己的 `UINT64`/`UINT32`。

## 不用 Pin 时会怎样

`native/CMakeLists.txt` 的 `PINBRIDGE_WITH_PIN` 默认 **OFF**。
这时仍然会构建两个不依赖 Pin 的目标：

* `pinbridge_abi_selfcheck`（布局断言）与 `bridge_roundtrip_test`；
* `pinbridge_bridge.dll` —— 导出 Hub 期望的 tool ABI 并转发给真实 agent，
  因此"Hub ↔ agent"链路在**没有 Pin 的机器上也能被真实驱动**（见
  [native/README.md](../../native/README.md)）。

Hub 侧通过 `pinbridge-sys` 的运行时加载器访问这些 DLL：DLL 不存在时只会得到
一条明确的错误，**不会让整个仓库构建失败**。

这条边界的意义：CI 与新人上手不应该被一个几百 MB 的第三方包挡住，
但"缺 Pin"必须是一眼能看懂的失败，而不是一堆链接错误。
