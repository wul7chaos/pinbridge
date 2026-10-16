//! 控制面 → 目标端的**主动请求**通道。
//!
//! ## 为什么需要一个 trait
//!
//! Hub 的核心逻辑（会话、控制权、能力闸门、观察窗口）必须在**没有真实目标、
//! 也没有 socket** 的测试里跑起来。而 `pb_memory_read` 这类"主动请求"一旦
//! 直接持有 `pinbridge_client::Client`，Hub 的每个测试就都要一个能通话的对端。
//!
//! 所以这里只声明"我需要什么"，具体是 socket、命名管道还是假的实现由上层决定。
//! Hub 侧保留的仍然是**闸门与语义**：能力检查、会话状态检查、错误翻译。
//!
//! ## 为什么 trait 要求 `Debug`
//!
//! `HubService` 自身是 `Debug`，会话条目里放 `Box<dyn TargetLink>` 之后，
//! 编译期就会逼着实现方给出一个能在错误信息里出现的名字。这不是装饰：
//! 出问题时"链路是什么、有没有挂上"是最需要立刻看清的一件事。

use pinbridge_core::error::Result;

/// 一条能把请求送到目标端的链路。
///
/// 实现方（例如 `pinbridge-mcp` 里基于 `pinbridge_client` 的适配器）负责
/// 序列化、超时与把链路层错误翻译成 [`pinbridge_core::error::Error`]。
pub trait TargetLink: std::fmt::Debug {
    /// 读取目标内存。
    ///
    /// # 约定
    ///
    /// - `length == 0` 由调用方拒绝（参数校验先于状态判断），实现方不必处理；
    /// - **一次读取的上限由链路层自己把关**：超限时必须明确报错，
    ///   不允许悄悄截短——"我要 64 KiB，你给我 4 KiB 且不说"会让上层
    ///   把半截数据当完整数据用；
    /// - 返回的字节数必须**恰好等于** `length`，否则就是 `Error::Protocol`。
    ///
    /// # Errors
    ///
    /// 链路不可用、目标拒绝、或返回长度不符时出错。
    fn read_memory(&mut self, address: u64, length: u32) -> Result<Vec<u8>>;

    /// 这条链路的自我描述（用于错误信息与审计）。
    fn describe(&self) -> String;
}
