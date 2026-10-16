# 控制权与权限模型

这是本项目最需要一次读懂的一篇。它回答三个问题：

1. **谁现在说了算？**（控制权）
2. **它能做什么？**（能力集）
3. **做过的每一件事在哪能查到？**（溯源）

## 一、人与 AI 是平级主体

```rust
pub enum PrincipalKind { Human, Ai, System }
```

不存在"AI 只能通过人类代理"的层级。AI 可以自己建会话、自己装脚本、
自己下断点——只要人类给过它对应的能力。

为什么这样定：插件式方案把 AI 放在"外挂脚本"的位置，主导权天然在人手里，
于是 AI 无法规划一个跨越多次交互的分析流程（例如脱壳那种
"先观测 → 再判断 OEP → 再决定 Dump 时机"）。要让它主导，就得先承认它是主体。

**唯一的不对称**：`capability_grant`（授予能力）与 `takeover`（接管控制权）
只对人类开放。这不是"人比 AI 高一等"，而是"阀门必须在人的手里"——
一个能给自己扩权的 agent，其权限边界等于没有。

## 二、能力集：只能被显式扩大，永远会被上限约束

```rust
Capability::ALL = [mem_read, mem_write, reg_write, breakpoint, single_step,
                   syscall_hook, pattern_hook, exception_steer, trace_record,
                   process_control, script_load, capability_grant]
```

| 主体 | 初始能力集 | 说明 |
|------|-----------|------|
| 人类 `human:*` | `operator_default`（全部 12 位） | 操作员即权威 |
| AI `ai:*` | `read_only_baseline` = `mem_read \| trace_record` | **默认只读** |
| 系统 `system` | `operator_default` | 定时器、收尾流程 |

三条规则：

1. **默认最小**。AI 一进来只能读，不能写、不能下断点、不能装脚本。
2. **授权是绝对设定，但受会话上限（ceiling）约束**：
   `granted = requested ∩ ceiling`。ceiling 由人类单独调（`set_ceiling`），
   收窄时**同步作用于所有被授权方**——不需要逐个追回已发出的授权。
3. **权威自身的权限不被 ceiling 自动回收**。否则会出现"能力棘轮"：
   人类把 ceiling 收到最小之后，连 `capability_grant` 都没了，
   再也调不回来，工具被锁死。

> 这两条在 `crates/pinbridge-hub/src/hub.rs` 里都有对应的单测
> （`grants_are_bounded_by_the_session_ceiling`、`ai_starts_read_only_and_cannot_escalate_itself`）。

### 位序是契约

`Capability::bit()` 用 `Capability::ALL` 的**下标**做位序，短名（`name()`）
出现在 JSON、CLI 与 C 头文件里。改位序或改短名都属于**破坏性变更**。

> 踩过的坑（已修）：早期把枚举判别值当位掩码用（`Self::TraceRecord as u32 == 8`，
> 被当成 `1 << 3 = Breakpoint`），结果是"只读基线"实际包含了断点权限。
> 现在有一条测试专门断言位掩码必须由 `bit()` 计算。

## 三、控制权：带 epoch 的租约

同一时刻只有一个主体掌握控制权（单写者）。租约是：

```rust
ControlLease { holder, capabilities, epoch, expires_at_ms, state }
```

* **epoch 单调递增**，只有 `renew`/移交会推进；
* 任何写操作都必须携带它签发时的 epoch；
* **旧 epoch 的命令会被拒绝**（`ControlConflict`），而不是"尽力执行"。

为什么非要有 epoch：AI 是长任务执行者。假设它正在加载一套 hook 脚本，
人类中途按了"接管"并手工改了内存。如果 AI 的在途命令不失效，它下一秒就会
覆盖人类刚建立的现场，而两边都认为自己工作正常。epoch 把这种冲突
从"静默破坏"变成了"显式报错"。

### 生命周期的四种动作

| 动作 | 谁能做 | 效果 |
|------|--------|------|
| 建会话 | 有 `process_control` 的主体 | 租约归创建者，epoch = 1 |
| 续租 | 持有者 | 刷新过期时间，epoch 不变 |
| `takeover` | **仅人类** | 收回旧租约 + epoch+1 |
| 过期 | 系统 | 惰性判定，`expires_at_ms` 到点即失效 |

过期是**惰性**的（在下一次访问时判定），因为一个休眠的目标进程里
不该有后台线程在跑心跳。

## 四、三关校验：能力 → 控制权 → 状态

每个会改变目标的请求都要依次通过：

```text
1. 能力（CapabilitySet::require）     ← 最便宜，可以在本地直接拒绝
2. 控制权（epoch == lease.epoch）     ← 需要持有租约快照
3. 会话状态（SessionState 允许该动作） ← 最贵，要查会话
```

顺序是刻意的：把拒绝做在链路最前面，可以让"AI 疯狂重试无权限操作"
**不产生任何对目标的副作用**。

### 会话状态机

```text
Created ──attach──▶ Attached ──run──▶ Running ⇄ Paused
   │                   │                 │        │
   └──────fail────────▶ Failed            └─stop───┴──▶ Detached ──▶ Closed
```

两条容易被忽略的规则：

* `Running` 时**不接受新脚本**（要先 pause）：在目标正在跑的时候换脚本，
  会让"这段行为是哪一版脚本决定的"变得无法回答；
* 非法迁移返回错误而不是静默忽略。静默忽略会让 AI 以为命令生效了。

## 五、双端校验

能力在**两个地方**各校验一次：

```text
Hub（crates/pinbridge-hub） ──▶ AgentClient（客户端侧） ──▶ Agent（目标进程内）
       能力 ∩ ceiling            能力位检查                  能力位检查
```

不是冗余：任何一端被绕过（工具链 bug、手工构造的报文、将来可能出现的本地提权）
都不应该直接拿到目标进程的写权限。

## 六、溯源：为什么失败也要记

`ProvenanceLog` 是只追加的，记录 `{seq, at, session, actor, action, detail, outcome}`，
同时写进 `.pbt` 的 AUDIT 段。

* 成功要记：谁装的是哪一版脚本（sha256）、谁下的断点、谁开始的录制；
* **失败也要记**：AI 的失败尝试是理解它"为什么走到了当前策略"的唯一线索。

不记失败的话，你会看到"AI 突然开始用另一种方法"，而完全不知道它试过什么、
为什么放弃。

## 七、人类怎么"随时接管"

```bash
# 工作台里点"接管"，或：
# 1. 人类拿到新 epoch
# 2. AI 的下一次写操作会收到 ControlConflict，错误里带 holder 与两个 epoch
```

MCP 层还会把这个错误翻译成可执行的建议：

> 控制权可能在别处被接管（当前持有者 human:operator，当前 epoch 3，
> 你手里的 epoch 是 1）。请重新 attach 获取新 epoch 后重试。

这就是"可恢复的失败"与"无法理解的失败"的区别。AI 拿到这句话就知道该做什么，
而不是开始瞎试。
