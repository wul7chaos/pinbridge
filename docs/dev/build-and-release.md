# 构建与发布

## 日常构建

```bash
cargo build --workspace                 # debug
cargo build --workspace --release       # release（LTO thin、panic=abort）
cargo build --profile release-with-debug   # 需要符号的 release
```

`.cargo/config.toml` 里预置了常用别名：

```bash
cargo b       # build --workspace --all-targets
cargo c       # check --workspace --all-targets
cargo t       # test  --workspace --all-targets
cargo lint    # clippy --workspace --all-targets --all-features -- -D warnings
cargo gate    # fmt-check + lint + test + doc（本地门禁）
```

## 产物

| 产物 | 来源 | 用途 |
|------|------|------|
| `pinbridge` | `crates/pinbridge-cli` | 人用的命令行（tools / replay / demo / mcp） |
| `pinbridge-mcp` | `crates/pinbridge-mcp` | MCP stdio 服务 |
| `pinbridge_agent.dll` | `crates/pinbridge-agent`（cdylib） | 注入目标进程的 agent |
| `pinbridge.dll` | `native/`（`PINBRIDGE_WITH_PIN=ON`） | Pin tool（C++），**需要 Pin SDK** |
| `pb`（Python 包） | `python/` | 离线分析 + `pb-replay` |

**注意**：`pinbridge.dll` 与 `pinbridge_agent.dll` 的关系是
"Pin tool 加载 agent"。两者的 ABI 由 [../specs/c-abi.md](../specs/c-abi.md) 约束，
版本必须匹配（不匹配时加载器拒绝加载）。

## 发布流程

### 1. 决定版本号

语义化版本，但注意**三个版本是独立演进的**：

| 版本 | 位置 | 何时提升 |
|------|------|----------|
| 包版本 | `Cargo.toml` 的 `[workspace.package] version` | 每次发布 |
| `.pbt` 格式 | `FORMAT_VERSION` | 读侧无法兼容的改动 |
| C ABI | `PB_ABI_VERSION` | 结构体布局或导出面变化 |

用户看到的可能是"0.3.0 + 格式 v1 + ABI v2"——文档里要写清楚。

### 2. 更新 CHANGELOG

把 `[Unreleased]` 的内容归到新版本号下，写清：

* 新增 / 变更 / 修复 / 已知限制；
* **不兼容变更**（含迁移方式）：格式版本、ABI 版本、能力位、CLI 输出格式。

### 3. 门禁全跑

```bash
pwsh scripts/verify.ps1
cargo deny check
```

外加 **M1 之后的手工检查项**（无法自动化，见 [testing.md](testing.md) 最后一节）：

- [ ] 附加普通 x64 程序：事件流正常、目标不卡顿
- [ ] 目标设置 TF：`trap_flag` 事件出现，异常位置与原生一致
- [ ] 目标长循环：轨迹可折叠，`dropped` 如实上报
- [ ] 目标崩溃：读侧报"不完整"而不是"损坏"
- [ ] 人类 takeover：AI 的下一次写操作收到 `control_conflict` 且带 hint

### 4. 打 tag 并推送

```bash
git switch main && git pull
git tag -a v0.1.0 -m "PinBridge 0.1.0：工程骨架与格式规范"
git push origin v0.1.0
```

`.github/workflows/release.yml` 会：

1. 在 Windows 与 Linux 上构建；
2. **跑冒烟测试**（`pinbridge version` + `pinbridge tools`）——
   发布物必须能跑，这是最低要求；
3. 打包（二进制 + README/LICENSE/CHANGELOG）为 zip；
4. 从 `CHANGELOG.md` 抽取本版说明创建 Release（找不到条目会失败）。

### 5. 发布后

* 在 `CHANGELOG.md` 顶部加回空的 `[Unreleased]` 段；
* 如果这次提升了格式或 ABI 版本，在 `docs/` 相关规范里把"当前版本"更新，
  并在 `SECURITY.md` / `README.md` 的兼容性说明里同步；
* 通知下游（如果有集成方）。

## 兼容性承诺

| 组件 | 承诺 |
|------|------|
| `.pbt` 格式 | **v1 稳定**。同主版本内向后兼容读取；`FORMAT_VERSION` 变化会在 Release 说明里显著标注 |
| C ABI | 同 `PB_ABI_VERSION` 内二进制兼容；加载器**拒绝**不匹配的版本（不尝试兼容） |
| MCP 工具 | 工具名稳定；新增工具不破坏旧客户端；**移除工具属于破坏性变更** |
| CLI 输出 | 人类可读部分可能变化；脚本依赖请用 `--json`（M1 提供） |

"拒绝不匹配"是刻意的：跨版本尽力兼容在这种工具里会制造"看起来能用但行为微妙不同"
的状态，比直接失败危险得多。

## 交叉编译与平台

| 平台 | 状态 |
|------|------|
| Windows x64 | ✅ 主目标（Pin 支持最好） |
| Linux x64 | ⚠️ Rust 部分可构建可测试；Pin 侧待验证 |
| Windows x86（32 位目标） | ⚠️ 需要 Pin 的 32 位 JIT 在独立进程里运行，M2 规划 |
| macOS | ❌ 不支持（Pin 不提供） |

`.cargo/config.toml` 里对 MSVC 目标设置了 `crt-static`，
避免把 `vcruntime` 依赖带进目标进程（目标机上不一定有）。
