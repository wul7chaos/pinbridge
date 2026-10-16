<!--
  PR 模板。
  写清"为什么"比"改了什么"重要得多：diff 已经说明了改了什么，
  而三个月后没人能从 diff 里还原出当时的判断依据。
-->

## 这个 PR 做了什么

<!-- 一到三句话。如果标题已经说清，这里可以不重复。 -->

## 为什么这样做

<!-- 关键：替代方案是什么，你为什么没选它。涉及权限/协议/格式时尤其必要。 -->

## 影响面

- [ ] 触及 `.pbt` 格式（→ 必须同步改 `FORMAT_VERSION` + Rust/Python 两侧 + 黄金字节用例）
- [ ] 触及 C ABI（→ 必须同步改 `PB_ABI_VERSION` + `native/include/pinbridge.h` + `docs/specs/c-abi.md`）
- [ ] 触及 MCP 工具表或能力位（→ 必须同步改 `docs/specs/mcp-tools.md` / 授权矩阵）
- [ ] 新增/升级依赖（→ 已在 `docs/dev/dependency-policy.md` 允许范围内，并说明理由）
- [ ] 纯文档改动（可跳过多项门禁）

## 验证方式

<!-- 贴上你实际跑过的命令与关键输出。不要写"应该没问题"。 -->

```text
$ cargo test --workspace
...
$ cargo run -p pinbridge-cli -- demo
...
```

## 自查清单

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 通过
- [ ] `cargo test --workspace` 通过
- [ ] `cargo run -p pinbridge-cli -- demo` 通过（端到端没被破坏）
- [ ] Python 侧 `pytest -q` 通过（若改了轨迹/事件相关代码）
- [ ] 新代码有对应测试；**失败的用例不是被删除而是被解释**
- [ ] 未实现的能力返回显式错误，没有静默成功
- [ ] 任何丢弃事件的路径都计数并上报
- [ ] `CHANGELOG.md` 已更新（代码改动必填）
- [ ] 文档中的命令/路径与实现一致

## 关联

<!-- Fixes #123 / Refs #456 -->
