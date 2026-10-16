# ADR 0010：探针层用 Intel Pin，但把它当作**可替换件**

* 状态：已接受（2026-10-16）
* 相关：[0001 插装框架优于调试 API](0001-instrumentation-over-debug-api.md)、
  [0004 Rust 内核与窄 C ABI](0004-rust-core-narrow-c-abi.md)

## 背景

实现插装需要一个 JIT DBI 基座。ADR 0001 已经选定"插装框架"这条路，
但没有回答一个后来变得很现实的问题：**用哪一个，以及万一它不能用怎么办。**

这个问题的由来是两条具体的事实：

1. Intel Pin 在 AMD CPU 上"能跑，但官方不支持"。社区里报告过 SSE4.2 检测异常
   之类的现象（同一份 tool 在 Intel 上正常）。本项目的开发机就是 AMD。
2. Pin 是闭源、按 Intel 自己的许可分发，不能进仓库；CI 也拿不到它。

第一条如果处理不好，会让整个项目的可用性绑死在一个"不承诺支持你的 CPU"的
第三方组件上；第二条已经迫使 M0.5 先做了一层不依赖 Pin 的桥接。

## 决定

**继续用 Intel Pin 3.31 作为探针层，但把它限制在一个可替换的边界内。**

具体地：

* 探针层 = `native/src/pinbridge_tool.cpp`（Pin tool）与后续的
  `flags_virtualize.cpp`。**只有这两个文件**允许出现 `pin.H`。
* 其余所有东西——事件模型、`.pbt` 格式、能力闸门、事件队列、脚本宿主、
  录制的落盘（**包括写出 `.pbt` 的动作**）——都在 agent（Rust）里，
  通过 C ABI 通信。
* Pin tool 只做三件事：注册探针、把观察到的事实推给 agent、开始/结束录制。
  它**不**决定权限，也**不**写文件格式。

## 实测记录（2026-10-16）

在当前开发机上（AMD Ryzen 7 9700X，Windows）：

```text
third_party\pin\pin.exe -version
  → Pin: pin-3.31-98869-fa6f126a8
third_party\pin\pin.exe -- cmd /c echo pin-can-launch
  → pin-can-launch（exit 0）
pin.exe -t pinbridge.dll -- pb_sample_target.exe
  → 入口点命中 0x7FF65A311310（序号 1）→ 事件穿过桥接层与 agent → .pbt 落盘 → 我们的读取器解析通过
```

也就是说：**在这个 AMD 平台上，Pin 3.31 的启动、插桩、事件回传这条链是通的**。
这不是"AMD 被支持"的证明（Intel 从未承诺），但它把"能不能用"从传闻变成了
一条可复现的检查。任何人换机器时跑一遍上面三行即可得到自己的结论。

## 备选（若某台机器上 Pin 不可用）

按优先级：

1. **DynamoRIO**（BSD、官方支持 AMD64）：定位与 Pin 相同（指令级 JIT DBI），
   替换面同样是"探针层"。
2. **QBDI**：开源、与 Frida 集成好，适合需要脚本化 Hook 的场景。
3. **TinyInst**（Google Project Zero）：轻量，面向覆盖率引导的 fuzz。

替换 Pin 需要改动的**只**是探针层：agent、事件模型、`.pbt`、能力闸门、
Hub、MCP 全部不动。这正是上面那条"只有两个文件允许出现 pin.H"的价值——
它不是洁癖，而是为了让这个 ADR 的备选方案真的可行。

## 为什么不现在就换

* 现在换是**在没有任何失败证据的情况下**更换基座：实测链路是通的。
* Pin 的 `INS_/RTN_/IMG_` 探针粒度与我们要做的系统调用、内存访问、模板匹配
  四类插桩点匹配得很好，DynamoRIO 的对应 API 需要在探针层重写一遍。
* 真到了必须换的那天，代价已经被边界限制住了（见上）。

## 后果

* **好**：CPU 兼容性风险从"项目级"降到"探针层级"；CI 与新人上手不需要 Pin。
* **代价**：探针层不能依赖 Pin 之外的框架特性；跨边界只有 C ABI 一条路，
  推事件的成本要接受（因此高频数据靠 agent 内部写入 `.pbt`，而不是逐条过边界）。
* **需要守住的约束**：任何人想图省事把"权限判断"或"文件格式"挪进 Pin tool，
  都是在破坏这个 ADR——那会让备选方案重新变得不可行。
