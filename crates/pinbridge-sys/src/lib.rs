//! PinBridge 的 C ABI 类型层。
//!
//! # 这一层为什么存在
//!
//! Intel Pin 的工具是 C++ DLL，它按 Pin 的约定被 `pin.exe -t <tool>` 加载。
//! 而 PinBridge 的分析能力与业务逻辑主要用 Rust 写（内存安全、依赖管理、
//! 与 Hub 共享同一套类型）。二者之间的边界必须**极其窄且极度稳定**：
//!
//! ```text
//! pin.exe
//!   └─ pinbridge.dll （C++，Pin tool；探针/JIT/上下文控制/TF 虚拟化）
//!         └─ 按 C ABI 加载并调用 pinbridge_agentdll.dll（Rust，本仓库的 pinbridge-agent）
//! ```
//!
//! 窄边界的实际含义：**只传整数、指针 + 长度、以及定长结构体**。
//! 不传 C++ 对象、不传 `std::string`、不让 Rust 的 panic 穿过 FFI
//! （所以 release 配置里 `panic = "abort"`，见根 `Cargo.toml`）。
//!
//! # 关于 `unsafe`
//!
//! 本 crate 与 `pinbridge-agent` 是全仓库唯一允许 `unsafe` 的两个地方
//! （根 `Cargo.toml` 的 `[workspace.lints.rust]` 里 `unsafe_code = "deny"`）。
//! 允许的理由写在这里而不是散落在各处：**FFI 边界本身就是 unsafe 的定义域**，
//! 把它藏进 safe 包装里只会让人忘记"这里有一次跨语言调用"。
//! 每个 `unsafe` 块仍然必须写清成立前提。

#![allow(unsafe_code)]
#![warn(missing_docs)]

pub mod abi;
pub mod loader;

pub use abi::{PbEventRaw, PbSet, PbStatus, Status};
pub use loader::{load, Api};
