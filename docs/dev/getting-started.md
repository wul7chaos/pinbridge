# 上手

目标：**15 分钟内跑通端到端自检**。你不需要 Intel Pin，也不需要任何样本。

## 前置

| 需要 | 版本 | 为什么 |
|------|------|--------|
| Rust | 仓库钉住 `1.98.1`（`rust-toolchain.toml`） | 工具链版本漂移会让 clippy 结果不一致 |
| Python | >= 3.10（可选） | 只跑 Rust 侧不需要它；跨语言一致性测试需要 |
| CMake | >= 3.20（可选） | 只做 `native/` 层时需要 |
| Intel Pin | 3.31（M1 才需要） | 见 [third_party/pin/README.md](../../third_party/pin/README.md) |

## 三条命令

```bash
git clone https://github.com/wul7chaos/pinbridge
cd pinbridge

cargo test --workspace            # 139 个单测，全部离线
cargo run -p pinbridge-cli -- demo    # 端到端自检
```

`demo` 会打印这样的过程（这是"骨架通不通"的最快判据）：

```text
== 1. 人类建立会话 ==
   session=1 pid=4242 epoch=1
== 2. AI 以只读身份尝试下断点（应当被拒）==
   已拒绝: 能力不足: 需要 Breakpoint，当前持有 CapabilitySet(257)
== 3. 人类授权（能力只能不超过会话上限）==
   已授予: mem_read|mem_write|...|capability_grant（能力集位图 4095）
== 4. 下断点 + 加载脚本 ==
   script=1 sha256=d9e5d86c4403
== 5. 录制轨迹并灌入模拟事件 ==
   轨迹: .../session-1-<时间戳>.pbt
   原始 4069 条 → 记录 71 条（折叠 3999 条，折叠率 0.983）
== 6. 折叠查询（AI 的观察视角）==
   #1 state_change ... #8 repeat repeats=6
== 7. 读回轨迹并校验可逆性 ==
   OK: 71 条记录无损还原为 4069 条事件
== 8. 操作溯源 ==
   {... "action":"grant_capabilities" ...}
自检通过。
```

注意第 2 步：**只读主体被拒绝**是预期行为。如果它没被拒绝，
说明权限链路坏了——这是最需要立刻修的一类问题。

## 一键准备环境（可选）

```powershell
pwsh scripts/bootstrap.ps1 -InstallHooks
```

它会：检查/安装 Rust 工具链 → 准备 Python 环境（优先用 `uv` 建隔离 venv）
→ 配置 git（提交模板、行尾策略、`rerere`）→ 安装 hooks（`commit-msg` / `pre-commit`）。

脚本是幂等的，反复跑不会破坏已有环境；每一步都会打印"做了什么、为什么需要"。

## native 层（C++ 桥接，可选但推荐）

这一层**不需要 Intel Pin**：它构建出 `pinbridge_bridge.dll`——导出 Hub 期望的
tool ABI 并转发给真实 agent DLL 的桥接层。有了它，"Hub ↔ agent"这条链路
在没有 Pin 的机器上也能被真实驱动（见 [dev roadmap](../roadmap.md) 的 M0.5）。

```powershell
pwsh scripts/build-native.ps1            # 配置 + 构建 + ctest
pwsh scripts/build-native.ps1 -RequireAgent   # agent DLL 缺失即失败
```

脚本自己找 Visual Studio / Ninja / CMake（没有 CMake 时用 `uv tool install cmake` 拉一个）。
跑完之后可以验证 Hub 侧加载器：

```powershell
$env:PINBRIDGE_REQUIRE_NATIVE = '1'
$env:PINBRIDGE_BRIDGE_DLL = "$PWD/native/build/pinbridge_bridge.dll"
cargo test -p pinbridge-sys --test bridge_end_to_end -- --nocapture
```

没有 C++ 工具链也能开发：`cargo test --workspace` 会跳过那条集成测试，
其余全部离线可跑。CI 的 `native` job 负责保证它**不被永久跳过**。

## 提交前

```powershell
pwsh scripts/verify.ps1            # Windows
bash scripts/verify.sh             # Linux / macOS / Git Bash
```

五个步骤与 CI 一一对应：格式 → clippy（`-D warnings`）→ 测试 →
端到端 demo → Python 侧。

## 常见问题

**Q: `cargo test` 提示找不到某个 crate 的 Cargo.toml？**
工作区成员是显式列在根 `Cargo.toml` 的 `members` 里的。新增 crate 时
必须同时加进 `members` 与 `[workspace.dependencies]`（后者用于让所有成员
用 `x.workspace = true` 引用）。

**Q: clippy 报了一堆 pedantic 警告？**
先确认你用的是仓库钉住的工具链（`rustup show`）。`clippy.toml` 与
根 `Cargo.toml` 的 `[workspace.lints.clippy]` 里已经逐条豁免了
本领域不适用的项，并写明了理由；**不要为了过 CI 去加新的 allow**，
除非你能在 PR 里写清理由（见 [coding-standards-rust.md](coding-standards-rust.md)）。

**Q: 没有 Pin 能做什么？**
几乎全部。M0 的骨架、测试、CLI、Python 侧、轨迹格式、权限模型、MCP 工具表
全都离线可跑。需要插装的动作会显式返回 `not_supported`
（而不是静默成功），这是刻意设计的失败方向。

**Q: 想接 AI 客户端试试？**

```jsonc
{
  "mcpServers": {
    "pinbridge": { "command": "pinbridge-mcp", "args": ["--trace-dir", "/tmp/pinbridge-traces"] }
  }
}
```

M0 阶段 `pb_session_create` 之类需要能力的工具默认不可见（AI 只有只读能力），
所以你能看到工具列表、能调 `pb_session_list` / `pb_provenance`，
但写操作需要人类先授权（`pb_capability_grant`）。

**Q: 为什么 `pb_events_query` 返回的 `args` 比 8 项少？**
JSON 层去掉了末尾的零（二进制里是定长 8 项）。缺失的项按 0 理解。

## 下一步读什么

* 想理解设计：[../architecture/overview.md](../architecture/overview.md)
* 想改代码：[workflow.md](workflow.md) + [../architecture/components.md](../architecture/components.md)
* 想知道"为什么不用 X"：[../adr/README.md](../adr/README.md)
* 想做对抗：[../design/injection-points.md](../design/injection-points.md)
