//! `pinbridge_agentdll`：跑在**目标进程内部**的那一半。
//!
//! # 职责
//!
//! 1. 承接 `pinbridge.dll`（C++ / Pin tool）通过 C ABI 传来的调用；
//! 2. 维护有界事件队列、序号分配与录制器；
//! 3. 宿主内嵌 CPython，把脚本回调跑在目标线程上；
//! 4. 在**进程内**做能力校验——Hub 侧的校验不算数（见下）。
//!
//! # 为什么 agent 侧还要再校验一次能力
//!
//! Hub 与 agent 之间只隔一条回环 TCP。任何一端被绕过（工具链 bug、
//! 手工构造的报文、将来可能出现的本地提权）都不应该直接拿到目标进程的
//! 写权限。所以 `pb_set_capabilities` 之后，**每一个**导出入口都会先查一次能力位。
//!
//! # 当前实现范围（M0）
//!
//! * 完整：ABI 导出面、能力校验、事件队列、序号分配、录制器接线、错误字符串；
//! * 桩实现：所有需要 Pin 运行时支撑的动作（断点、单步、内存读写、脚本执行）
//!   返回 [`Status::NotSupported`]，并在 `README` 与 `docs/roadmap.md` 的 M1 里跟踪。
//!
//! 把"还没有的部分"做成**显式失败**而不是"静默空操作"，是本项目的一条硬规矩：
//! 静默空操作会让 AI 以为某件事已经生效，从而在错误的前提上继续推理。
//!
//! # `unsafe` 说明
//!
//! 本 crate 与 `pinbridge-sys` 是全仓库仅有的两个允许 `unsafe` 的地方（见根
//! `Cargo.toml` 的 lint 配置）。这里的 `unsafe` 全部集中在"解引用 C 指针"
//! 与"导出 C ABI 函数"两件事上，每个块都注明成立前提。

#![allow(unsafe_code)]
// cdylib 导出符号时链接器会打印"正在创建库 ... .lib/.exp"，这是正常的不是问题
#![allow(linker_messages)]
#![warn(missing_docs)]

pub mod exports;
pub mod host;
pub mod link;
pub mod modules;
pub mod script;

pub use host::{AgentHost, AgentState};
pub use link::LinkServer;
pub use script::{ScriptBudget, ScriptHost, ScriptPolicy, ScriptState};
