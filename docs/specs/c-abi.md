# C ABI 规范

两份孪生声明必须逐字段一致：

| 侧 | 文件 |
|----|------|
| C/C++ | [`native/include/pinbridge.h`](../../native/include/pinbridge.h) |
| Rust | [`crates/pinbridge-sys/src/abi.rs`](../../crates/pinbridge-sys/src/abi.rs) |

它约束两条边界：

```text
pin.exe
  └─ pinbridge.dll（C++ / Pin tool）──C ABI──▶ pinbridge_agentdll.dll（Rust，crates/pinbridge-agent）
  └─ Hub 侧通过 pinbridge-sys 运行时加载 pinbridge.dll
```

## 硬性约束

1. 只允许**定长整数、指针 + 长度、定长数组、`repr(C)` 结构体**；
2. **不允许 `bool`**（虽然 C 与 C++ 都是 1 字节，但跨语言的布局假设是经典事故源）；
3. **不允许变长结构**穿过边界；
4. **不允许异常/panic 穿过边界**：C++ 侧不抛，Rust 侧 release 配置 `panic = "abort"`
   且每个导出入口都做了捕获（返回状态码而不是让进程死掉）；
5. 字符串是 **UTF-8 + NUL 结尾**，生命周期由调用方保证。

## 版本

`PB_ABI_VERSION = 1`。加载器（`pinbridge-sys::loader`）的流程是：
**先 `pb_abi_version()` 校验，再解析其余符号**。

版本不符时**拒绝加载**，不做"能调就调"的乐观尝试——
一个错位的函数指针在插装场景下会直接把目标进程打崩，
而崩因会指向目标程序本身，排查代价极高。

## 状态码

| 值 | 名称 | 含义 |
|----|------|------|
| 0 | `PB_STATUS_OK` | 成功 |
| -1 | `PB_STATUS_INVALID_ARGUMENT` | 参数非法（空指针、长度越界、UTF-8 非法） |
| -2 | `PB_STATUS_NOT_ATTACHED` | 尚未附加到目标 |
| -3 | `PB_STATUS_CAPABILITY_DENIED` | 能力不足 |
| -4 | `PB_STATUS_NOT_SUPPORTED` | 当前平台/Pin 版本不支持（**M0 的桩函数全部走这里**） |
| -5 | `PB_STATUS_BUSY` | 目标正忙 |
| -6 | `PB_STATUS_TIMEOUT` | 超时 |
| -7 | `PB_STATUS_INTERNAL` | 内部错误 |
| -8 | `PB_STATUS_ABI_MISMATCH` | ABI 版本不匹配 |

**这些数值一旦发布不可更改**（C 侧可能已按数值分支）。
新增只能往下排（-9、-10…）。

## 能力位

位序来自 **`Capability::ALL` 的下标**，不是枚举定义的顺序，
也不是"看起来合理"的顺序。位序与短名改动都属于破坏性变更。

| 位 | 名称 | 短名 |
|----|------|------|
| 0 | `PB_CAP_MEM_READ` | `mem_read` |
| 1 | `PB_CAP_MEM_WRITE` | `mem_write` |
| 2 | `PB_CAP_REG_WRITE` | `reg_write` |
| 3 | `PB_CAP_BREAKPOINT` | `breakpoint` |
| 4 | `PB_CAP_SINGLE_STEP` | `single_step` |
| 5 | `PB_CAP_SYSCALL_HOOK` | `syscall_hook` |
| 6 | `PB_CAP_PATTERN_HOOK` | `pattern_hook` |
| 7 | `PB_CAP_EXCEPTION_STEER` | `exception_steer` |
| 8 | `PB_CAP_TRACE_RECORD` | `trace_record` |
| 9 | `PB_CAP_PROCESS_CONTROL` | `process_control` |
| 10 | `PB_CAP_SCRIPT_LOAD` | `script_load` |
| 11 | `PB_CAP_CAPABILITY_GRANT` | `capability_grant` |

只读基线 = `mem_read | trace_record` = **257**。
`native/src/abi_selfcheck.cpp` 里有 `static_assert` 钉住这个值——
一旦有人把它改成包含写能力，"AI 默认最小权限"的承诺就失效了。

## 事件结构体布局

```c
typedef struct pb_event_raw {
  uint64_t sequence;      // offset  0
  uint8_t  kind;          // offset  8
  uint8_t  reserved[3];   // offset  9   ← 显式填充，不用隐式填充
  uint32_t thread_id;     // offset 12
  uint64_t address;       // offset 16
  uint64_t args[8];       // offset 24
} pb_event_raw;           // sizeof = 88, alignof = 8
```

**为什么显式填充**：不同编译器/标准库对隐式填充的处理在跨语言边界上并不可靠。
显式写出填充字节，可以让 `static_assert` 的断言真正有意义。

这组数字被三处断言钉住：

1. `abi.rs` 的单测（`event_layout_is_explicit_and_stable`）；
2. `native/src/abi_selfcheck.cpp` 的 `static_assert`（编译期）；
3. 本文件。

## 导出函数

### agent 侧（Rust 导出，C++ 调用）

| 函数 | 所需能力 | M0 状态 |
|------|----------|---------|
| `pb_abi_version` | — | 完整 |
| `pb_attach` / `pb_detach` / `pb_is_attached` | — | 完整（状态标记） |
| `pb_set_capabilities` / `pb_get_capabilities` | — | 完整 |
| `pb_last_error` | — | 完整 |
| `pb_allocate_sequence` / `pb_push_event` / `pb_drain_events` / `pb_stats` | — | 完整 |
| `pb_kind_name` | — | 完整 |
| `pb_start_trace` / `pb_stop_trace` | `trace_record` | 完整 |
| `pb_memory_read` | `mem_read` | **桩** |
| `pb_memory_write` | `mem_write` | **桩** |
| `pb_set_breakpoint` | `breakpoint` | **桩** |
| `pb_single_step` / `pb_continue` / `pb_pause` | `single_step` / `process_control` | **桩** |
| `pb_load_script` | `script_load` | **桩** |

### 桩函数的行为约定

**先校验能力，能力通过之后再返回 `PB_STATUS_NOT_SUPPORTED`。**

顺序不能反。如果桩函数直接返回"不支持"，那么"能力校验是否真的生效"
在 M0 阶段就无法被测试覆盖；等 M1 把桩换成真实现时，这条链路会**第一次**
被真正执行——那是最不适合发现权限漏洞的时机。
（`crates/pinbridge-agent/src/exports.rs` 的 `abi_surface_smoke` 就是在测这个顺序。）

### tool 侧（C++ 导出，Hub 通过 `pinbridge-sys` 加载）

`pb_abi_version` / `pb_start` / `pb_attach` / `pb_set_capabilities` /
`pb_continue` / `pb_pause` / `pb_drain_events` / `pb_last_error`。

签名与 `crates/pinbridge-sys/src/loader.rs` 的函数指针表一一对应。
**解析失败时的错误信息必须带上具体缺失的导出名**——
"DLL 加载失败"在排查 Pin 版本差异时毫无价值。

## 指针契约

| 参数 | 允许 NULL | 行为 |
|------|-----------|------|
| `pb_push_event(raw)` | 是 | `INVALID_ARGUMENT` |
| `pb_drain_events(out, capacity, written)` | 是 | `INVALID_ARGUMENT`；`capacity == 0` 同样拒绝 |
| `pb_memory_read(..., out, out_capacity, out_written)` | 是 | `INVALID_ARGUMENT`；`length > out_capacity` 拒绝 |
| `pb_load_script(name, source)` | 是 | `INVALID_ARGUMENT`；非 UTF-8 同样拒绝 |
| `pb_last_error()` | 否（永不返回 NULL） | 指针在**下一次写入错误状态前**有效，调用方必须立刻拷贝 |

`is_null` 检查优先于能力检查：**空指针是调用方的 bug，不是权限问题**，
把它报成 `CAPABILITY_DENIED` 会让排查方向完全走错。

## 变更流程（必须逐条走完）

1. 改 `native/include/pinbridge.h` 与 `crates/pinbridge-sys/src/abi.rs`（同一提交）；
2. 若有布局变化，提升 `PB_ABI_VERSION` 并更新 `abi_selfcheck.cpp` 的 `static_assert`；
3. 更新本文件与 `CHANGELOG.md`（在"不兼容变更"下写明迁移方式）；
4. 更新 Rust 侧单测与 `pinbridge-agent` 的导出测试；
5. 在 PR 描述里勾选"触及 C ABI"并请第二个人确认。

**只改一侧的 PR 一律打回**：跨语言结构体不一致是最难排查的一类崩溃，
而它的根本原因往往就是"顺手改了一边"。
