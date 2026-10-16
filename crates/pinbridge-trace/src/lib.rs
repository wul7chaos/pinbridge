//! 事件模型、折叠压缩与轨迹格式。
//!
//! 这一层是 PinBridge 的"黑匣子"：agent 在目标进程里采集出来的东西，
//! 经过它折叠、编码、落盘，再由 Python 侧离线解码/replay。
//!
//! **双向一致性是硬约束**：同一份 .pbt 必须能被 Rust 与 Python 两侧解出
//! 完全相同的事件序列。因此格式规范（`docs/specs/pbt-format.md`）先写，
//! 两侧实现都只是规范的转写，并且两边都有"同一份字节流"的黄金用例测试。
//!
//! 为什么不直接存 JSON：VMP 对抗场景下 trace 可以到几十 GB，
//! JSON 的字段名开销在这里是纯粹的浪费（`"address":` 这 11 个字节，
//! 每条记录都要写一次）。二进制 + varint + delta 是这个量级下的唯一选择。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod diff;
pub mod event;
pub mod fold;
pub mod pbt;
pub mod varint;

pub use diff::{diff_traces, diff_warnings, DiffOptions, Difference, Divergence, TraceDiff};
pub use event::{register_name, Event, EventKind, MAX_ARGS, REGISTER_COUNT, REGISTER_NAMES};
pub use fold::{expand_records, expand_records_checked, FoldOutcome, RepeatFolder};
pub use pbt::{PbtReader, PbtWriter, Summary};
pub use varint::{read_u64, try_read_u64, write_u64, write_zigzag};
