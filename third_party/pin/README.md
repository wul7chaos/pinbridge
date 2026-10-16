# Intel Pin（第三方）

PinBridge 选择插装框架作为基座（理由见
[docs/adr/0001-instrumentation-over-debug-api.md](../../docs/adr/0001-instrumentation-over-debug-api.md)），
Intel Pin 是其中的闭源代表。**Pin 按 Intel 自己的许可分发，不包含在本仓库里**，
`third_party/pin/` 目录在 `.gitignore` 中被排除。

## 为什么不入库

1. **许可**：Intel Pin 的许可不允许我们把它再分发到本仓库；
2. **体积**：解压后几百 MB，塞进 git 会让每次 clone 都变慢；
3. **可复现性**：我们在文档里钉死版本号与校验和，比把二进制塞进历史里更可审计。

## 获取（M1 阶段需要）

1. 到 Intel 的 Pin 下载页获取 **3.31** 对应的包
   （`pin-3.31-<build>-gcc-windows.zip` 或 `msvc` 变体）。
2. 解压到 `third_party/pin/`，目录结构应为：

   ```text
   third_party/pin/
     pin.exe
     source/tools/Pin/          ← PIN_ROOT 指向这里
       pin.H
       ...
     source/tools/ManualExamples/
   ```

3. 记下版本与 sha256，填到下面这张表里（提交 PR 时更新）：

   | 平台 | 版本 | sha256 | 记录人 |
   |------|------|--------|--------|
   | Windows x64 | 3.31-<build> | `<sha256>` | （待填） |
   | Linux x64 | 3.31-<build> | `<sha256>` | （待填） |

4. 验证：

   ```bash
   pwsh scripts/fetch-pin.ps1 -Verify     # 只检查存在性与版本，不下载
   ```

## 构建工具 DLL

```bash
cmake -S native -B native/build -DPINBRIDGE_WITH_PIN=ON -DPIN_ROOT="$PWD/third_party/pin/source/tools/Pin"
cmake --build native/build --config Release
```

产物 `native/build/dist/pinbridge.dll`。用法：

```bash
third_party/pin/pin.exe -t native/build/dist/pinbridge.dll -- TARGET.exe
```

## 不用 Pin 时会怎样

`native/CMakeLists.txt` 的 `PINBRIDGE_WITH_PIN` 默认 **OFF**，
此时只构建一个不依赖 Pin 的 ABI 自检目标（`native/src/abi_selfcheck.cpp`）。
Hub 侧通过 `pinbridge-sys` 的运行时加载器访问 `pinbridge.dll`：
DLL 不存在时只会得到一条明确的错误，**不会让整个仓库构建失败**。

这条边界的意义：CI 与新人上手不应该被一个几百 MB 的第三方包挡住，
但"缺 Pin"必须是一眼能看懂的失败，而不是一堆链接错误。
