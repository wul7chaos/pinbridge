# 开发流程

## 分支模型：主干开发 + 短生命周期分支

**不用 Git Flow**。理由：这是一个工具项目，不是有明确发版窗口的产品。
`develop` / `release/*` / `hotfix/*` 这套在两人规模下只会带来合并成本，
而且会让"main 长期不可发布"变成常态。

```text
main ────●────●────●────●────●────▶  永远可发布；每个提交都过 CI
          \        /
           ●──●──●     feat/xxx（< 3 天，diff < 400 行最佳）
```

规则：

| 项 | 约定 |
|----|------|
| 主干 | `main`，保护分支，禁止直接 push |
| 分支名 | `feat/` `fix/` `docs/` `refactor/` `chore/` + 简短英文（kebab-case） |
| 分支寿命 | **目标 < 3 天**。超过一周的分支基本等于一次小重构，请拆 |
| 合并方式 | **squash merge**（PR 标题即最终提交信息） |
| 合并后 | 删除分支；若被 revert，用 `revert` 提交而不是强推 |
| 发版 | 在 `main` 上打 tag（`v0.1.0`），见 [build-and-release.md](build-and-release.md) |

**没有独立 release 分支**：发布产物由 tag 触发 CI 构建，不从分支切。

## 一次改动的完整流程

### 1. 先有 issue

除了明显的笔误修正，任何改动都要有 issue。issue 里必须写清：

* 要解决什么（**困境**，不是方案）；
* 验收标准（最好是可执行的判断：一条命令、一个断言）。

"加一个 `get_oep` 接口"不是验收标准；"脱壳流程里确认 OEP 的调用次数从 6 降到 2"
才是。

### 2. 分支与提交

```bash
git switch -c feat/trace-repeat-folding
# 小步提交，遵循 Conventional Commits
git commit -m "feat(trace): 折叠连续重复事件为 EVENT_REPEAT 记录"
```

提交信息规范见 [commit-convention.md](commit-convention.md)。
本地 hooks 会拦不合规的提交（`scripts/bootstrap.ps1 -InstallHooks` 安装）。

**提交粒度**：一个提交 = 一次可解释的逻辑改动。以下情况必须分开：

* 格式化 / 重命名 与 行为改动；
* 依赖升级 与 功能改动（见 [dependency-policy.md](dependency-policy.md)）；
* 协议 / 格式变更 与 使用它的功能。

原因是可回溯：出问题时 `git bisect` 只在"每个提交都语义单一"时有用。

### 3. 自测

```bash
pwsh scripts/verify.ps1        # 五步与 CI 一一对应
```

**不允许**在 PR 里写"应该没问题"。请贴出实际跑过的命令与关键输出。

### 4. 提 PR

PR 模板会要求你勾选影响面。涉及以下任一项时，**必须有第二个人的显式确认**：

* `.pbt` 格式（`FORMAT_VERSION`）
* C ABI（`PB_ABI_VERSION`）与 `native/include/pinbridge.h`
* 能力位 / MCP 工具表 / 授权矩阵
* 权限判断的任何代码路径

### 5. 评审

评审人请优先看这些（顺序即优先级）：

1. **失败方向对不对**：未实现时是显式报错还是静默成功？丢弃事件有没有计数？
2. **权限有没有漏**：新动作是否登记了所需能力？是否在 Hub 与 agent 两端都校验？
3. **跨语言是否同步**：改了 Rust 侧类型，Python/C 侧跟了吗？黄金字节要改吗？
4. **测试是否覆盖失败路径**：只测 happy path 的 PR 会被要求补用例。
5. 可读性与命名。

**评审意见必须可操作**："这段会在 X 情况出错，因为 Y" ；不要"这写得不好"。

### 6. 合并

squash merge。合并前确认：

- [ ] CI 全绿（含 `pinbridge demo` 与 Python 侧）
- [ ] `CHANGELOG.md` 有 `[Unreleased]` 条目（CI 会检查代码改动是否伴随）
- [ ] 文档里的命令/路径与实现一致
- [ ] 没有把 `samples/`、`*.pbt`、密钥带进来

## 例外流程

| 情况 | 做法 |
|------|------|
| 紧急修复（main 坏了） | 直接开 `fix/` 小 PR，跳过 issue，但必须在当天补上 issue 与 CHANGELOG |
| 大重构 | 先写 ADR（`docs/adr/`），评审通过后再开分支；分支可以长一点，但要保持 main 可发布 |
| 实验性代码 | 放独立 crate 或 feature flag，**默认关闭**；不要用注释掉的代码代替 |
| 回滚 | `git revert`（保持历史），然后在 CHANGELOG 里写明原因 |

## 关于"失败的测试"

* 不允许为了让 CI 变绿而删除测试。若测试确实过时，请在 PR 里解释**为什么它不再成立**；
* 允许 `#[ignore]`，但必须附一句说明与跟踪 issue 号；
* 修 bug 的 PR 应当**先加一个能复现的失败用例**，再修——这是唯一能证明
  "真的修好了"的方式。

## 与 CI 的三处门禁

| 门禁 | 位置 | 内容 |
|------|------|------|
| 本地 | `scripts/verify.ps1` / `verify.sh` / githooks | 同一套清单的两个入口 |
| PR | `.github/workflows/ci.yml` | fmt / clippy / test / e2e / python / changelog / commit-lint |
| 每周 | `.github/workflows/deny.yml` | 依赖公告与许可证 |

**修改门禁必须同时改这三处**，否则会出现"本地过了 CI 挂了"这种最浪费时间的状态。
