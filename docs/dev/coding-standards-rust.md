# Rust 代码规范

规范不是审美偏好。本项目的 Rust 代码有两个特殊约束：

1. **它会跑在别人的进程里**（`pinbridge-agent`），崩了不是"报个错"而是"目标没了"；
2. **它与 C++ 和 Python 共享数据格式**，靠"约定"维持一致是不可靠的。

因此下面每一条都尽量给出"为什么"。

## 编辑器与工具链

| 工具 | 配置 |
|------|------|
| 工具链 | `rust-toolchain.toml` 钉死（当前 `1.98.1`）。**不要用别的版本提 PR** |
| 格式 | `rustfmt.toml`（只用 stable 支持的选项，避免每天一堆 nightly 警告） |
| Lint | 根 `Cargo.toml` 的 `[workspace.lints]` + `clippy.toml` |
| 编辑器 | 建议 `.editorconfig` + `cargo fmt on save`；`rust-analyzer` 用仓库工具链 |

## Lint 策略

```toml
[workspace.lints.rust]
unsafe_code = "deny"          # 只有 sys / agent 显式 allow
missing_docs = "warn"
unreachable_pub = "warn"

[workspace.lints.clippy]
all = "warn"        # 严格门禁（CI: -D warnings）
pedantic = "warn"   # 采纳，但逐条豁免不适合本领域的项
nursery = "allow"   # 官方定义即"仍在开发中、允许误报"，不适合当门禁
```

**加新的 allow 是有代价的**：必须在 PR 里写清"这条 lint 为什么在本项目不适用"，
并加在 `[workspace.lints.clippy]` 的豁免清单里（那里已经有 20 多条，每条带理由）。
不要在代码里散布 `#[allow(...)]` —— 那会让读者无法判断"这是全局策略还是局部妥协"。

已豁免的几条及理由（摘录）：

| lint | 为什么豁免 |
|------|-----------|
| `missing_const_for_fn` | const 化会改变 API 形状，收益在本项目不明显 |
| `cast_possible_wrap` / `cast_possible_truncation` | 地址/寄存器在 u64↔i64 间来回是本领域常态，且有测试兜底 |
| `match_same_arms` | 能力表/状态机里"结果相同但语义不同"很常见，合并会丢信息 |
| `doc_markdown` | 文档里大量 Pin/DynamoRIO/x64dbg 专名会误报 |

## 类型设计

### 用强类型 ID，不要裸 `u64`

```rust
// ❌ 传参写反在编译期抓不出来
fn attach(session: u64, script: u64) -> Result<()>;

// ✅
fn attach(session: SessionId, script: ScriptId) -> Result<()>;
```

见 `pinbridge-core/src/id.rs`。这条在"把 AI 脚本装到了错误的会话"这类
事故里能直接救命。

### 新增能力位必须同步四处

```text
1. Capability::ALL          （位序来源）
2. Capability::name()       （IPC/CLI/审计里的短名）
3. docs/specs/ipc-protocol.md 与 docs/architecture/control-ownership.md
4. native/include/pinbridge.h 的 PB_CAP_*（若 C 侧需要）
```

### 错误类型

* 库层统一用 `pinbridge_core::error::Error`（`thiserror`）；
* **错误信息要可归因**：能回答"是权限不够、目标不在、还是通道满了"；
* 跨 FFI 边界不用 `Error`，用状态码 + `pb_last_error()`。

## 并发与资源

* **通道必须有界**（`pinbridge_core::channel::bounded`）。`clippy.toml` 里
  把 `std::sync::mpsc::Sender`/`SyncSender` 列为禁止类型；
* **丢弃必须计数**。任何"悄悄丢事件"的实现都视为缺陷；
* **锁要短**。`agent` 里的全局 `Mutex` 在探针热路径上被持有，
  锁内不允许做大块分配、I/O 或回调；
* 锁中毒要恢复（`unwrap_or_else(|e| e.into_inner())`），
  否则一次 panic 会让 agent 永久锁死，整个目标进程的分析全部失效。

## `unsafe` 规则

只有两个 crate 允许：`pinbridge-sys`、`pinbridge-agent`（crate 文档里写了理由）。
其余 crate 用 `#![forbid(unsafe_code)]` 钉死。允许的地方：

1. **每个 `unsafe` 块必须写成立前提**（`// SAFETY: ...`），
   前提要具体到"由调用方保证 ptr 以 NUL 结尾且在本次调用期间存活"这个粒度；
2. **每个 `unsafe fn` 必须有 `# Safety` 文档段**；
3. 不允许用 `unsafe` 绕过借用检查（`transmute` 生命周期的活儿要能解释清楚）。

## 文档注释

* **公开项必须有文档**（`missing_docs = warn`）。
* 文档要写"**为什么**"，而不是重复签名。反面例子：

  ```rust
  /// 获取会话 ID。
  pub fn session_id(&self) -> SessionId { ... }   // 没信息量
  ```

  正面例子：

  ```rust
  /// 取会话 ID。
  ///
  /// 之所以单独提供一个访问器而不是把字段设为 `pub`：未来 ID 一旦需要在
  /// 跨进程边界上重新映射（M1 的多会话代理），调用点不需要改。
  pub fn session_id(&self) -> SessionId { ... }
  ```

* 模块级文档（`//!`）要说清这个模块**不做什么**，这比说它做什么更有用。

## 测试

* 单测放在同文件 `#[cfg(test)] mod tests`；跨模块行为放 `tests/`；
* 命名说明**行为与条件**：`stale_epoch_commands_are_rejected_after_takeover`；
* 断言必须给消息（失败时能一眼看懂）：

  ```rust
  assert_eq!(folded.len(), 2, "1000 条相同连续事件应折叠成 2 条");
  ```

* 测试**不许**为了通过而改断言。见 [testing.md](testing.md)。

## 提交前

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 常见的"看起来更聪明"的写法（别写）

| 别写 | 为什么 |
|------|--------|
| `panic!("unreachable")` 在 FFI 路径上 | release 下 `panic = "abort"` 会直接杀掉目标进程 |
| `unwrap()` 在处理外部输入时 | 轨迹文件是不可信输入，见 [../specs/pbt-format.md §9](../specs/pbt-format.md) |
| 为了省一次 clone 用 `unsafe` | 本项目的性能瓶颈在插装，不在这些地方 |
| `impl Default` 只是为了让 clippy 闭嘴 | `Default` 是有语义的：它代表"合理的初始状态" |
| 吞掉错误（`let _ = ...`） | 至少加注释说明"为什么可以忽略" |
