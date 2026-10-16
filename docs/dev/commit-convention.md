# 提交信息规范

采用 [Conventional Commits 1.0.0](https://www.conventionalcommits.org/zh-hans/)。
本地由 `scripts/githooks/commit-msg` 拦截，CI 由 `commit-lint` 任务复检。

## 格式

```text
<type>(<scope>): <subject>

<body>

<footer>
```

### type

| type | 用法 | 会不会出现在 CHANGELOG |
|------|------|------------------------|
| `feat` | 新功能（对使用者可见） | ✅ |
| `fix` | 缺陷修复 | ✅ |
| `perf` | 性能优化（语义不变） | ✅ |
| `refactor` | 重构（既非新功能也非修复） | ❌ |
| `docs` | 仅文档 | ❌ |
| `test` | 仅测试 | ❌ |
| `build` | 构建系统 / 依赖 / 工具链 | ❌（依赖升级会单独记） |
| `ci` | CI 配置与脚本 | ❌ |
| `chore` | 杂项（版本号、脚手架） | ❌ |
| `revert` | 回滚既有提交 | ✅ |

### scope

用**模块名**而不是文件名：

```text
core proto hub client mcp cli trace agent sys abi
python desktop scripts docs ci repo deps pin
```

不知道该用哪个时不写 scope（`feat: ...` 也可以），但**不要自己发明**——
scope 的价值在于能被 `git log --grep` 精确检索。

### subject

* 祈使句，现在时（"折叠…"而不是"折叠了…"）；
* **不超过 72 个显示列**（中文按 2 列计，所以一个纯中文标题大约 30 字以内）；
* 结尾不加句号；
* 超长就把细节移到 body 里——标题只留"做了什么"这一个动作。

这条长度限制由 `scripts/githooks/commit-msg` 在本地强制，CI 的 `commit-lint` 复检。
hook 按 UTF-8 结构计算显示列（不依赖 locale），因此中文标题不会被误判为超长。

### body

写**为什么**，不写"改了什么"（diff 已经说明改了什么）。每行 ≤ 100 字符。
以下情况必须写 body：

* 改变了对外行为（协议、格式、权限、CLI 输出）；
* 做了一个看起来奇怪但必要的实现选择；
* 修了一个**有代表性**的 bug（写清根因，避免同类错误再犯）。

### footer

```text
BREAKING CHANGE: <不兼容变更与迁移方式>
Fixes #123
Refs #456
Co-authored-by: Name <mail@example.com>
```

**提到不兼容变更时，`BREAKING CHANGE:` 是必须的**，并且要在
`CHANGELOG.md` 的"不兼容变更"下同步一条。

## 好例子

```text
feat(trace): 折叠连续重复事件为 EVENT_REPEAT 记录

录制线程此前对每个事件都写一条记录。VMP 单指令 spin 场景下轨迹体积
膨胀约 3 个数量级（一次 5 分钟会话可达几十 GB）。

现在仅当「序号连续且载荷完全一致（不比序号）」时才折叠，段尾写一个
标记记录：sequence 落在段内最后一条上，args[0] 是折叠条数，
args[1] 是原始类型。展开是逐字节可逆的，已有跨语言黄金用例覆盖。

Closes #41
```

```text
fix(core): 只读基线误含断点权限

`read_only_baseline` 之前把枚举判别值当位掩码用
（TraceRecord 的判别值是 8，被当成 1<<3 = Breakpoint），
导致"默认只读"的 AI 实际拿到了断点能力。

改为统一用 Capability::bit() 计算，并加测试断言位掩码来源。

BREAKING CHANGE: 能力位图数值不变，但 read_only_baseline() 的返回值
从 8 变为 257。任何按数值硬编码只读基线的调用方需要改用该方法。
Fixes #57
```

## 坏例子

| 提交信息 | 问题 |
|----------|------|
| `update code` | 无 type、无信息量 |
| `修复了一些问题` | 不说修了什么、不说是哪一类 |
| `feat: 加了一个功能` | subject 无信息量，等于没写 |
| `WIP` | 不要提交半成品到共享分支（本地随便，推之前 squash 掉） |
| `fix(hub)：修复控制权` | 用了全角冒号（hooks 与 CI 都会拦） |
| `feat(trace): 折叠重复事件为 EVENT_REPEAT 记录，同时把 hub 的窗口容量从 1 万改成 2 万，另外升级了 serde` | 一个提交做了三件事，`git bisect` 会失效 |

## 与 CHANGELOG 的关系

CI 的 `changelog` 任务会检查：**代码目录有改动时 `CHANGELOG.md` 必须有条目**。

```text
改动的路径里出现 crates/ native/ python/src/ apps/  → 必须动 CHANGELOG.md
只动 docs/ 或测试                                  → 可以跳过
```

需要跳过时（例如纯 CI 脚本调整），在 PR 描述里说明理由即可——
这条守卫的目的是防止"功能悄悄上线但没人知道"，不是给人添麻烦。

## 需要修订已推送的提交时

```bash
# 只改最近一条
git commit --amend

# 改多条（分支上还没合并时）
git rebase -i main

# 已经合并进 main 的：不要改历史，用 revert + 新提交
```

`main` 的历史是**不可改写**的。需要更正时开一个新提交，
并在 body 里说明"更正了 <hash>"。
