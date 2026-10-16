# 常见问题

## 定位

**Q: 这和 x64dbg + MCP 插件有什么本质区别？**

两个区别：

1. **基座**：我们用插装（Pin）而不是调试器框架。目标检测不到"被调试"，
   而且我们能做到指令级控制而不依赖被分析进程的异常路径；
2. **AI 的位置**：插件方案里 AI 是外挂（主导权在人手里，只能调固定接口）。
   我们让 AI **写脚本注入目标进程**，它能分支、试错、回退。
   MCP 工具因此只有 11 个，而且刻意保持薄。

**Q: 和 Frida 比呢？**

Frida 更轻、更成熟、上手更快。我们选 Pin 的理由是：在 VMP 指令级对抗里，
我们需要"探针 + 代码缓存 + 上下文控制"这一套更底层的可控性；
另外脚本语言选 Python 而不是 JS（AI 生成二进制处理代码时，
Python 生态与生成质量更好）。Frida 在快速定位类任务上可能更好用——
我们不做"全面替代"的主张。

**Q: 为什么不做一个"AI 一键脱壳"？**

因为脱壳是**对抗**，不是流程。壳在变，一键方案第二天就失效。
我们提供的是"AI 能自主探索"的基础设施 + "结论可复现"的证据格式，
而不是一个固定的魔法按钮。

## 使用

**Q: 没有 Intel Pin 能用吗？**

能用**绝大多数**：所有单测、CLI、`.pbt` 读写与离线分析、MCP 工具表、
权限模型都可以离线跑。需要插装的动作（内存读写、断点、单步、脚本执行）
会显式返回 `not_supported`。

这是刻意的：CI 与新人上手不该被一个几百 MB 的第三方包挡住。

**Q: `cargo run -p pinbridge-cli -- demo` 是什么？**

端到端自检：建会话 → AI 越权尝试（应被拒）→ 人类授权 → 装脚本 →
录轨迹 → 折叠查询 → 读回校验 → 打印溯源。它是判断"骨架有没有被改坏"
最快的方式，也在 CI 里跑。

**Q: 怎么把 PinBridge 接给 AI 客户端？**

```jsonc
{
  "mcpServers": {
    "pinbridge": {
      "command": "pinbridge-mcp",
      "args": ["--trace-dir", "/tmp/pinbridge-traces"]
    }
  }
}
```

M0 阶段 AI 默认只有 `mem_read` + `trace_record`，
所以 `tools/list` 只会暴露它允许使用的工具（看不到写操作）。
人类需要通过工作台/CLI 用 `pb_capability_grant` 授权。

**Q: 为什么 AI 看不到全部工具？**

让 AI 反复尝试它做不到的事，既费 token 又容易产生"换个办法试试"的幻觉。
**把边界摆在明面上**比让它在边界上撞墙更好。安全上不依赖这一点——
Hub 会独立校验能力。

## 设计

**Q: 为什么"未实现"不返回一个空结果，而是报错？**

因为 AI 会把静默成功当成"已生效"，然后在错误的前提上继续推理。
一个显式错误会让它换策略；一个空结果会让它继续往前走直到结论全错。

**Q: 为什么 `dropped` 非零不直接算失败？**

那样会让"弱一点的目标"变得无法分析。我们的做法是**把前提大声说出来**：
"这份结论建立在可能丢过 X 条事件的基础上"。这比"失败"或"假装完整"都更实用。

**Q: 为什么 `.pbt` 不直接用 JSON？**

VMP 场景下 JSON 的字段名开销（`"address":` 11 字节 × 亿级记录）就是纯浪费；
而且没有 delta 编码，体积会差一到两个数量级。
详细对比见 [adr/0005](adr/0005-pbt-binary-format.md)。

**Q: 为什么要维护 Rust 和 Python 两个实现？**

离线分析用 Python（迭代快、生态好），在线路径必须用 Rust（内存安全、可注入）。
两者读同一份文件 → 必须保证一致 → 用**黄金字节**与**端到端互读**测试兜底。

**Q: 为什么 epoch 这么麻烦？**

因为执行者是 AI。它会在人类接管之后**继续按原计划执行**。
如果没有 epoch，"人类刚改的内存被 AI 覆盖"这种事既没有报错也无法归因。
详见 [adr/0008](adr/0008-control-lease-with-epoch.md)。

**Q: 为什么 clippy 豁免了那么多 pedantic 项？**

因为那些建议在本领域里会把代码改坏或改难读（例如 `cast_possible_wrap`：
地址在 u64↔i64 间来回是常态）。每一条豁免都在根 `Cargo.toml` 里写了理由。
**不要为了过 CI 加新的豁免**，除非你能在 PR 里写清理由。

## 开发

**Q: 我要改 `.pbt` 格式，从哪开始？**

1. 读 [specs/pbt-format.md](specs/pbt-format.md) 的 §8 变更流程；
2. 判断是否要提升 `FORMAT_VERSION`（读侧无法兼容 → 必须提升）；
3. 同时改 Rust 与 Python 两侧 + 黄金字节用例；
4. 更新文档 + `CHANGELOG.md`（`BREAKING CHANGE:`）；
5. 在 PR 里勾选"触及 `.pbt` 格式"（会要求第二个人确认）。

**Q: 我要加一个能力位？**

同步四处：`Capability::ALL`、`Capability::name()`、
`docs/architecture/control-ownership.md` 的授权矩阵、
`native/include/pinbridge.h` 的 `PB_CAP_*`（若 C 侧需要）。
别忘了加一条"位不冲突"的测试（已有 `every_capability_bit_is_distinct_and_within_bounds`）。

**Q: 为什么新增 crate 要改两处？**

根 `Cargo.toml` 的 `members`（工作区成员）与 `[workspace.dependencies]`
（让所有成员用 `x.workspace = true`）。漏了后者会报
`dependency.<name> was not found in workspace.dependencies`。

**Q: 测试挂了，但我觉得是测试写错了？**

先看规范怎么说（`docs/specs/`）。如果规范确实改了，那是一次**协议变更**，
要走变更流程；如果规范没变，那八成是实现的问题。
不允许"为了让 CI 变绿"改断言而不解释。

**Q: 为什么 CI 里要跑 `pinbridge demo`？**

因为单元测试全绿但端到端断了是常见状态（某处契约被改，两边各自自洽）。
`demo` 是唯一能覆盖"跨模块契约"的检查。

## 商业与法律

**Q: 可以用来分析恶意样本吗？**

可以，只要你有权分析（你自己的环境、你的样本）。请**不要**把样本提交到仓库——
`.gitignore` 里 `samples/` 是硬禁止，PR 里也会拦。

**Q: Intel Pin 的许可能用吗？**

PinBridge 采用 MIT，但 **Intel Pin 按 Intel 自己的许可分发**，
不包含在本仓库里，需要你自行获取（见
[third_party/pin/README.md](../third_party/pin/README.md)）。
请自行确认你的使用场景符合 Intel 的条款。

**Q: 报告安全问题怎么走？**

见 [SECURITY.md](../SECURITY.md)。能让 AI 越权、能让解析器崩溃、
会让目标进程被意外破坏的，请走私有渠道，不要开公开 issue。
