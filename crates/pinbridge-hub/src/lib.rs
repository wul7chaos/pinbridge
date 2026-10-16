//! HubService：控制面。
//!
//! ## 职责边界
//!
//! Hub **不做分析**。它只负责四件事：
//!
//! 1. **会话**：目标进程的生命周期与其状态机；
//! 2. **控制权**：谁现在说了算（epoch 语义见 `pinbridge-core::capability`）；
//! 3. **脚本生命周期**：谁在什么时候注入过哪一版代码（哈希留痕）；
//! 4. **观察窗口**：把事件通道与轨迹文件暴露成 AI 能读的东西。
//!
//! ## 为什么必须有"观察窗口"这一层
//!
//! 目标进程里的事件是真的会丢的（背压、通道容量、进程崩溃）。Hub 保留一个有界
//! 的**记录窗口**，并把"窗口之外已被丢弃的条数"如实上报。AI 如果拿不到这个数字，
//! 就会把"我看到的"当成"发生过的"——这是自动化分析里最危险的一类错觉。
//!
//! 完整历史只存在于 `.pbt`：所以"要下结论就必须先录轨迹"不是流程洁癖，
//! 而是唯一的真相来源。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod hub;
pub mod provenance;
pub mod recorder;
pub mod runtime;
pub mod script;

pub use hub::HubService;
pub use provenance::{AuditEntry, ProvenanceLog};
pub use recorder::TraceRecorder;
pub use runtime::SessionRuntime;
pub use script::{Script, ScriptRegistry, ScriptState};
