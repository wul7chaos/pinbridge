//! 重复折叠（trace 压缩的第一档，也是收益最大的一档）。
//!
//! ## 为什么需要它
//!
//! 在 VMP 对抗里，目标的外壳会把关键循环展开成"同一条指令 + 同一个地址 + 同样的
//! 参数"重复执行几十万到上亿次（变形、解密、校验）。按原样录制，一条 5 分钟的
//! 会话可以产出几十 GB 轨迹，其中 99.99% 是同一个字节序列——对人类没有信息量，
//! 对 LLM 更是纯粹的 token 灾难。
//!
//! ## 折叠语义（与 `docs/design/trace-compression.md` 保持一致）
//!
//! 一条事件只有同时满足下面两点，才会被折进当前重复段：
//!
//! 1. **序号连续**：`incoming.sequence == representative.sequence + folded + 1`
//! 2. **载荷相同**：[`Event::same_payload`]（类型、线程、地址、args[0..8] 全等，
//!    **不比序号**）
//!
//! 折叠结果：
//!
//! * 段的第一条原样输出（保留完整载荷，供重放时复制）；
//! * 后续被折叠的条数记在段尾的 [`EventKind::Repeat`] 标记里：
//!   `sequence = 段内最后一条的序号`、`args[0] = 被折叠的条数`、
//!   `args[1] = 原始类型`、线程与地址取自本段。
//!
//! 这个定义是**可逆**的：重放时遇到标记，按"复制上一条非标记事件的载荷"展开
//! `args[0]` 条，序号依次 +1，正好还原原始序列。可逆性是硬要求——
//! 压缩后的轨迹如果还原不出原时间线，就没有资格被用作分析证据。

use crate::event::{Event, EventKind};

/// 一次 `push` 产生的后果，供测试与统计使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldOutcome {
    /// 事件作为新重复段的第一条被输出。
    Started,
    /// 事件与当前段一致且序号连续，已被折进当前段（无输出）。
    Folded,
    /// 上一段结束：先输出折叠标记，再把当前事件作为新段的第一条输出。
    FlushedAndStarted,
}

/// 折叠器。**有状态**：它必须按序号递增的顺序喂入事件。
#[derive(Debug, Default)]
pub struct RepeatFolder {
    /// 当前重复段的代表事件（段内第一条）。
    representative: Option<Event>,
    /// 当前段已被折掉的条数（不含代表事件本身）。
    folded: u64,
    /// 本次会话累计折叠掉的条数（`flush` 不清零）。
    folded_total: u64,
    /// 单调性校验用：上一条输入事件的序号。
    last_input_sequence: u64,
}

impl RepeatFolder {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 累计折叠掉的条数。会话 summary 里的 `repeat_folded` 取自这里。
    #[must_use]
    pub const fn folded_total(&self) -> u64 {
        self.folded_total
    }

    /// 喂入一条事件。产生的输出通过 `sink` 回调，避免为每条事件分配 `Vec`。
    ///
    /// 回调而不是返回 `Vec<Event>` 的原因：录制线程每秒要处理百万级事件，
    /// 每条事件一次堆分配会让录制本身成为瓶颈（并且加剧目标进程的内存压力）。
    pub fn push<F: FnMut(Event)>(&mut self, event: Event, sink: &mut F) -> FoldOutcome {
        debug_assert!(
            event.sequence > self.last_input_sequence || self.last_input_sequence == 0,
            "事件序号必须严格递增：{} 之后收到 {}",
            self.last_input_sequence,
            event.sequence
        );
        self.last_input_sequence = event.sequence;

        let Some(representative) = self.representative else {
            self.representative = Some(event);
            sink(event);
            return FoldOutcome::Started;
        };

        let expected = representative
            .sequence
            .saturating_add(self.folded)
            .saturating_add(1);
        if expected == event.sequence && representative.same_payload(&event) {
            self.folded = self.folded.saturating_add(1);
            self.folded_total = self.folded_total.saturating_add(1);
            return FoldOutcome::Folded;
        }

        self.flush(sink);
        self.representative = Some(event);
        sink(event);
        FoldOutcome::FlushedAndStarted
    }

    /// 结束当前段。流结束时必须调用，否则最后一段的折叠计数会丢。
    pub fn finish<F: FnMut(Event)>(&mut self, sink: &mut F) {
        self.flush(sink);
    }

    fn flush<F: FnMut(Event)>(&mut self, sink: &mut F) {
        let Some(representative) = self.representative.take() else {
            return;
        };
        if self.folded == 0 {
            return;
        }
        let last_sequence = representative.sequence.saturating_add(self.folded);
        sink(Event::repeat_marker(
            last_sequence,
            self.folded,
            representative.kind,
            &representative,
        ));
        self.folded = 0;
    }
}

/// 折成一对多展开：把轨迹文件里的记录序列还原成原始事件序列。
///
/// 规则：遇到 [`EventKind::Repeat`] 标记时，取**上一条非标记事件**的载荷，
/// 依次生成 `args[0]` 条事件，序号从上一条递增到标记序号。
/// 标记自身不产生事件。
///
/// 这是"宽松"版本：不校验标记与上一条事件的序号是否自洽。
/// 需要把"轨迹被篡改"当成错误处理时用 [`expand_records_checked`]。
#[must_use]
pub fn expand_records(records: &[Event]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::with_capacity(records.len());
    for record in records {
        if record.kind != EventKind::Repeat {
            out.push(*record);
            continue;
        }
        let Some(previous) = out.last().copied() else {
            // 首条就是标记：文件损坏或被人为拼接过，宁可跳过也不猜。
            continue;
        };
        let mut sequence = previous.sequence;
        for _ in 0..record.arg(0) {
            sequence = sequence.saturating_add(1);
            let mut clone = previous;
            clone.sequence = sequence;
            out.push(clone);
        }
    }
    out
}

/// 严格版本：标记的序号必须等于"上一条事件序号 + 折叠条数"，
/// 否则说明轨迹自相矛盾（被改写、拼接或录制端有 bug），返回错误。
///
/// 取证场景（KCTF 复盘、样本对抗报告）必须走这个入口：一份自相矛盾的
/// 轨迹如果被静默接受，得出的结论会看起来完整但实际错误。
pub fn expand_records_checked(records: &[Event]) -> pinbridge_core::error::Result<Vec<Event>> {
    use pinbridge_core::error::Error;

    let mut out: Vec<Event> = Vec::with_capacity(records.len());
    for record in records {
        if record.kind != EventKind::Repeat {
            out.push(*record);
            continue;
        }
        let Some(previous) = out.last().copied() else {
            return Err(Error::Trace("轨迹以折叠标记开头，缺少代表事件".into()));
        };
        let count = record.arg(0);
        let expected = previous.sequence.saturating_add(count);
        if expected != record.sequence {
            return Err(Error::Trace(format!(
                "折叠标记自相矛盾：上一条序号 {} + 折叠 {} != 标记序号 {}",
                previous.sequence, count, record.sequence
            )));
        }
        if count == 0 {
            return Err(Error::Trace("折叠标记的条数为 0，应直接省略该标记".into()));
        }
        let mut sequence = previous.sequence;
        for _ in 0..count {
            sequence += 1;
            let mut clone = previous;
            clone.sequence = sequence;
            out.push(clone);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identical_events(start: u64, count: u64) -> Vec<Event> {
        (0..count)
            .map(|i| {
                Event::new(start + i, EventKind::SyscallEnter, 100, 0x7FF6_0000_2000)
                    .with_arg(0, 0x40)
                    .with_arg(1, 0x7FF6_0000_2300)
            })
            .collect()
    }

    fn fold(events: &[Event]) -> Vec<Event> {
        let mut folder = RepeatFolder::new();
        let mut out = Vec::new();
        for ev in events {
            folder.push(*ev, &mut |e| out.push(e));
        }
        folder.finish(&mut |e| out.push(e));
        out
    }

    #[test]
    fn a_long_run_collapses_to_two_records() {
        let events = identical_events(1, 1000);
        let folded = fold(&events);
        assert_eq!(folded.len(), 2, "1000 条相同连续事件应折叠成 2 条记录");
        assert_eq!(folded[0].sequence, 1);
        assert_eq!(folded[0].kind, EventKind::SyscallEnter);
        assert_eq!(folded[1].kind, EventKind::Repeat);
        assert_eq!(folded[1].sequence, 1000, "标记落在段内最后一条的序号上");
        assert_eq!(folded[1].arg(0), 999, "折叠条数不含代表事件");
        assert_eq!(folded[1].arg(1), u64::from(EventKind::SyscallEnter.code()));
        assert_eq!(folded[1].address, 0x7FF6_0000_2000);
    }

    #[test]
    fn fold_then_expand_is_lossless() {
        let events = identical_events(1, 500);
        let folded = fold(&events);
        assert_eq!(expand_records(&folded), events, "压缩必须可逆");
    }

    #[test]
    fn non_contiguous_sequence_breaks_the_run() {
        // 序号跳变（例如目标切了线程）即使载荷相同也不能折叠
        let mut events = identical_events(1, 5);
        for (i, ev) in events.iter_mut().enumerate().skip(2) {
            ev.sequence = 1 + (i as u64) * 2;
        }
        let folded = fold(&events);
        assert_eq!(folded.len(), 5, "序号不连续时必须逐条输出");
        assert_eq!(expand_records(&folded), events);
    }

    #[test]
    fn differing_payload_breaks_the_run() {
        let mut events = identical_events(1, 6);
        events[3].args[1] = 0xDEAD; // seq 4 的载荷与前后都不同
        let folded = fold(&events);
        let view: Vec<String> = folded.iter().map(Event::to_text).collect();
        assert_eq!(folded.len(), 5, "实际记录:\n{}", view.join("\n"));
        assert_eq!(folded[0].sequence, 1);
        assert_eq!(folded[1].kind, EventKind::Repeat);
        assert_eq!(
            (folded[1].sequence, folded[1].arg(0)),
            (3, 2),
            "前一段覆盖 seq 1..=3"
        );
        assert_eq!(folded[2].sequence, 4, "差异事件自身作为新段代表被保留");
        assert_eq!(folded[3].sequence, 5);
        assert_eq!(folded[4].kind, EventKind::Repeat);
        assert_eq!((folded[4].sequence, folded[4].arg(0)), (6, 1));
        assert_eq!(expand_records(&folded), events, "跨段还原也必须无损");
    }

    #[test]
    fn single_event_produces_no_marker() {
        let events = identical_events(7, 1);
        let folded = fold(&events);
        assert_eq!(folded, events);
        assert_eq!(folded.len(), 1);
    }

    #[test]
    fn outcome_reports_what_happened() {
        let mut folder = RepeatFolder::new();
        let events = identical_events(1, 3);
        let mut out = Vec::new();
        let a = folder.push(events[0], &mut |e| out.push(e));
        let b = folder.push(events[1], &mut |e| out.push(e));
        let mut other = events[2];
        other.address = 0x1000;
        let c = folder.push(other, &mut |e| out.push(e));
        assert_eq!(a, FoldOutcome::Started);
        assert_eq!(b, FoldOutcome::Folded);
        assert_eq!(c, FoldOutcome::FlushedAndStarted);
        assert_eq!(out.len(), 3, "代表事件 + 折叠标记 + 新段起点");
        assert_eq!(folder.folded_total(), 1);
    }

    #[test]
    fn expansion_without_a_previous_event_is_ignored() {
        let marker = Event::repeat_marker(10, 9, EventKind::BreakpointHit, &Event::EMPTY);
        assert!(
            expand_records(&[marker]).is_empty(),
            "孤立标记不应被展开成垃圾数据"
        );
    }

    #[test]
    fn checked_expansion_rejects_inconsistent_marker() {
        let events = identical_events(1, 2);
        // 人为篡改标记序号，使其与 "上一条序号 + count" 不一致
        let bogus = Event::repeat_marker(99, 5, EventKind::SyscallEnter, &events[0]);
        let records = vec![events[0], bogus];
        assert!(
            expand_records_checked(&records).is_err(),
            "不一致的标记必须被拒绝"
        );
    }
}
