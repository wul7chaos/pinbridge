# 0007. 主干开发（不用 Git Flow）

- 状态：已采纳
- 日期：2026-10-16
- 相关：[../dev/workflow.md](../dev/workflow.md)

## 背景

项目规模：两三人，一个工具，没有明确发版窗口，但要保证"任何人随时可以拿到
一份能跑的 main"。

Git Flow（`main` + `develop` + `release/*` + `hotfix/*` + `feature/*`）
是很多团队的默认答案，但它是为**有明确发布窗口、多版本并行维护**的产品设计的。

## 决策

**主干开发 + 短生命周期分支**：

```text
main ────●────●────●────●────●────▶  永远可发布
          \        /
           ●──●──●     feat/xxx（< 3 天）
```

* `main` 是保护分支，每个提交都过 CI；
* 分支名 `feat/` `fix/` `docs/` `refactor/` `chore/`；
* **squash merge**（PR 标题即最终提交信息）；
* 发布 = 在 `main` 上打 tag，产物由 tag 触发 CI 构建，**不切 release 分支**；
* 回滚用 `revert` 提交，不改写 `main` 历史。

## 考虑过的替代方案

| 方案 | 为什么没选 |
|------|-----------|
| Git Flow | 对两三人项目只有成本：`develop` 与 `main` 会长期分叉，`release/*` 在没有多版本并行需求时纯属仪式 |
| GitHub Flow + 长分支 + 部署预览 | 后端工具项目没有"部署预览"这一环节 |
| 在 `main` 上直接提交 | 失去 PR 评审与 CI 门禁；本项目有跨语言格式与权限这类"必须有人复核"的改动 |
| 每个 PR 独立 release 分支 | 同上，且会让 tag 与分支的对应关系变复杂 |

## 后果

**得到**：

* 只有一个长期分支 → 没有"合并 develop 到 main"这类仪式性冲突；
* CI 与门禁只有一条路径（PR）；
* 发布是**纯构建行为**（tag → 产物），可重复执行。

**付出与配套要求**：

* 必须保证 main 一直绿 → `main` 保护 + 必须过 CI + 本地 `scripts/verify.ps1`；
* 需要"随时可发布"的纪律 → 不合并半成品；实验性代码放 feature flag 或独立 crate；
* 需要 commit 规范与 CHANGELOG 守卫（否则发布时不知道写了什么）→
  见 [../dev/commit-convention.md](../dev/commit-convention.md) 与 CI 的 `changelog` 任务；
* 长分支是明确的坏味道 → 超过一周的分支应当拆成多个 PR。

**什么时候该重新考虑**：出现"需要同时维护两个大版本"的需求
（例如给出 0.1 与 0.2 都发补丁）。届时请新写一条 ADR 说明如何做
（多半是"从 tag 切维护分支"，而不是回到完整 Git Flow）。
