# 参与 PinBridge 开发

先说结论：**这个仓库不接受"我没跑过测试"的 PR**。理由很直白——
PinBridge 跑在别人的目标进程里，它出的问题不是"页面显示错了"，
而是"目标崩了/分析结论错了"。门槛只能靠纪律而不是靠好意。

---

## 一分钟上手

```bash
git clone https://github.com/wul7chaos/pinbridge
cd pinbridge

cargo test --workspace                     # 139 个单测，全部离线可跑
cargo run -p pinbridge-cli -- demo         # 端到端自检
cargo run -p pinbridge-cli -- tools        # 看一眼 MCP 工具表

cd python && pip install -e ".[dev]" && pytest -q
```

不需要 Pin，也不需要目标程序。需要插装的动作在 M0 阶段会显式返回
`not_supported`。

## 提交前必跑（与 CI 逐条对应）

```bash
cargo fmt --all -- --check                                   # 格式
cargo clippy --workspace --all-targets -- -D warnings         # lint（pedantic）
cargo test --workspace                                        # 单测
cargo run -p pinbridge-cli -- demo                            # 端到端
cd python && pytest -q                                        # 跨语言一致性
```

Windows 上可以用一条命令跑完：`pwsh scripts/verify.ps1`。

## 开发流程（简版）

我们的分支模型是**主干开发 + 短生命周期分支**，不是 Git Flow——
一个最多两人的工具项目引入 `develop`/`release`/`hotfix` 只会增加合并成本。

```text
main ────●────●────●────●────●────▶  永远可发布；每个提交都过 CI
          \        /
           ●──●──●      feat/xxx 短分支（< 3 天，< 400 行 diff 最佳）
```

1. **开 issue**（或在已有 issue 下认领），写清"要解决什么、验收标准是什么"。
2. **建分支**：`feat/`、`fix/`、`docs/`、`refactor/`、`chore/` + 简短英文描述。
3. **小步提交**：遵循 [Conventional Commits](docs/dev/commit-convention.md)，
   scope 用仓库里的模块名（`core`/`hub`/`trace`/`agent`/`mcp`/`docs`…）。
4. **提 PR**：填模板；说明"为什么"而不是"改了什么"（diff 已经说明改了什么）。
5. **评审**：至少一位维护者；涉及 ABI / 协议 / 轨迹格式的改动必须有第二个人的显式确认。
6. **合并**：squash merge，PR 标题即最终提交信息。合并后删除分支。

完整规则（含评审清单与例外流程）见 [docs/dev/workflow.md](docs/dev/workflow.md)。

## 硬性规则

这些不是建议，是 review 会直接打回的项：

| 规则 | 为什么 |
|------|--------|
| 不改 `.pbt` 格式就不动 `FORMAT_VERSION`；改了就**必须**同时改 Rust 与 Python 两侧实现 + 黄金字节用例 | 两个实现分叉会让"轨迹是唯一真相来源"这句话变成空话 |
| 不改 C ABI 就不动 `PB_ABI_VERSION`；字段增删必须同步 `native/include/pinbridge.h` 与 `docs/specs/c-abi.md` | 跨语言结构体不一致是最难排查的一类崩溃 |
| 新增能力位必须同步 `Capability::ALL`、`name()`、`docs/specs/ipc-protocol.md` 与授权矩阵 | 权限表漏一项 = 一个无声的越权入口 |
| 未实现的功能返回显式错误，**不允许**静默成功/静默空操作 | AI 会把"静默成功"当成"已生效"，之后的推理全错 |
| 任何丢弃事件的路径都必须计数并出现在 summary 里 | 同上：不能让"我看到的"冒充"发生过的" |
| `unsafe` 只允许出现在 `pinbridge-sys` 与 `pinbridge-agent`，且每块必须写成立前提 | 其余 crate 用 `#![forbid(unsafe_code)]` 把这条钉死 |
| 文档里的命令必须真能跑通（至少在本机） | 文档是给未来的自己和别人看的，跑不通的文档等于错的文档 |

## 目录职责

新增代码放哪里，看这一句话就够：

```text
业务规则/类型      → crates/pinbridge-core
线路协议/工具描述  → crates/pinbridge-proto
事件/轨迹          → crates/pinbridge-trace
会话/权限/脚本     → crates/pinbridge-hub
与目标进程通信     → crates/pinbridge-client、crates/pinbridge-sys
目标进程内运行     → crates/pinbridge-agent
对外接口           → crates/pinbridge-mcp、crates/pinbridge-cli、apps/desktop
离线分析           → python/
```

**依赖方向是单向的**：`core ← proto/trace ← hub ← {mcp, cli}`。
反向依赖（例如让 core 引用 hub）一律拒绝——那会让领域模型无法被 agent 复用。
新增依赖前先读 [docs/dev/dependency-policy.md](docs/dev/dependency-policy.md)。

## 报告安全问题

不要开公开 issue，见 [SECURITY.md](SECURITY.md)。

## 许可与出处

贡献即表示同意以 MIT 许可发布（见 [LICENSE](LICENSE)）。
若你的改动源自外部文章/代码，请在提交信息里写明出处——
本项目的起点就是一篇看雪文章，我们尤其在意出处是否清楚。
