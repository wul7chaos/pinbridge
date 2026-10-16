//! 会话运行时：事件通道、记录窗口与录制器。
//!
//! 三个概念在这里汇合，务必分清：
//!
//! | 载体 | 容量 | 用途 | 丢数据时 |
//! |------|------|------|----------|
//! | 事件通道 | 固定条数 | 推给脚本/上层的实时流 | 计数（`channel_dropped`） |
//! | 记录窗口 | 固定条数 | AI 按序号回看最近发生的事 | 计数（`window_dropped`） |
//! | `.pbt` 文件 | 磁盘 | 唯一完整的真相来源 | 不可能丢；写了就是写了 |
//!
//! 三者都会把"丢了多少"如实暴露出来。任何一个环节沉默地丢数据，
//! 都会让 AI 的分析结论变成"看起来有依据的错误答案"。

use std::collections::VecDeque;
use std::path::Path;

use pinbridge_core::channel::{bounded, BoundedReceiver, BoundedSender, DropPolicy};
use pinbridge_core::error::{Error, Result};
use pinbridge_core::id::EventSeq;
use pinbridge_trace::event::{Event, EventKind, MAX_ARGS};
use pinbridge_trace::fold::RepeatFolder;
use pinbridge_trace::pbt::Summary;

use crate::recorder::TraceRecorder;

/// 记录窗口默认容量（条数）。取值理由：一次典型的"看最近发生了什么"
/// 查询是几十条到几百条，1 万条足以覆盖 AI 的一轮迭代，同时把内存压在百 KB 量级。
pub const DEFAULT_WINDOW_CAPACITY: usize = 10_000;

/// 会话运行时。
#[derive(Debug)]
pub struct SessionRuntime {
    events: BoundedSender<Event>,
    inbox: BoundedReceiver<Event>,
    window: VecDeque<Event>,
    window_capacity: usize,
    window_dropped: u64,
    next_sequence: u64,
    recorder: Option<TraceRecorder>,
}

impl SessionRuntime {
    /// 按默认容量新建。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_WINDOW_CAPACITY)
    }

    /// 指定记录窗口容量。
    #[must_use]
    pub fn with_capacity(window_capacity: usize) -> Self {
        let (events, inbox) = bounded(65_536, DropPolicy::DropOldest);
        Self {
            events,
            inbox,
            window: VecDeque::with_capacity(window_capacity.min(4096)),
            window_capacity: window_capacity.max(1),
            window_dropped: 0,
            // 序号从 1 开始；0 保留给"未初始化"，避免和 `Event::EMPTY` 混淆
            next_sequence: 0,
            recorder: None,
        }
    }

    /// 记录一条事件，返回分配到的序号。
    ///
    /// 参数用 `&[(usize, u64)]` 而不是 `[u64; 8]`：调用点（agent 探针）
    /// 通常只知道 0~3 个参数，让调用方写满 8 个零是纯噪音。
    pub fn record(
        &mut self,
        kind: EventKind,
        thread_id: u32,
        address: u64,
        args: &[(usize, u64)],
    ) -> Result<EventSeq> {
        self.next_sequence += 1;
        let mut event = Event::new(self.next_sequence, kind, thread_id, address);
        for (index, value) in args {
            if *index >= MAX_ARGS {
                return Err(Error::Trace(format!("参数下标越界: {index}")));
            }
            event = event.with_arg(*index, *value);
        }
        self.push_event(event)?;
        Ok(EventSeq::new(event.sequence))
    }

    /// 推入一条已构造好的事件（测试与 agent 侧共用）。
    pub fn push_event(&mut self, event: Event) -> Result<()> {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.record(&event)?;
        }
        if self.window.len() >= self.window_capacity {
            self.window.pop_front();
            self.window_dropped += 1;
        }
        self.window.push_back(event);
        // 通道满了按 DropOldest 处理，丢弃计数由 channel 自己维护
        let _ = self.events.send(event);
        Ok(())
    }

    /// 当前记录窗口。
    #[must_use]
    pub fn window(&self) -> &VecDeque<Event> {
        &self.window
    }

    /// 窗口里最新一条的序号（没有事件时为 0）。
    #[must_use]
    pub fn last_sequence(&self) -> u64 {
        self.window.back().map_or(0, |e| e.sequence)
    }

    /// 已分配的最大序号（即使窗口已经滚掉该事件）。
    #[must_use]
    pub const fn next_sequence_hint(&self) -> u64 {
        self.next_sequence
    }

    /// 因窗口容量不足被滚掉的事件数。**非零意味着 `from` 太旧，答案不完整。**
    #[must_use]
    pub const fn window_dropped(&self) -> u64 {
        self.window_dropped
    }

    /// 因通道背压被丢弃的事件数。
    #[must_use]
    pub fn channel_dropped(&self) -> u64 {
        self.events.dropped()
    }

    /// 通道积压条数（脚本消费不过来时这个数字会持续增长）。
    #[must_use]
    pub fn backlog(&self) -> usize {
        self.inbox.len()
    }

    /// 按序号查询窗口内容。
    ///
    /// 返回 `(事件, 建议游标, 本窗口内是否因容量丢过数据)`。
    ///
    /// 注意折叠语义：这里的折叠是**在窗口切片上**做的，
    /// 如果一段重复横跨窗口边界，边界处那条的折叠计数不完整。
    /// 需要精确折叠统计时必须读 `.pbt`（`Summary::repeat_folded`）。
    #[must_use]
    pub fn query(&self, from: u64, limit: usize, fold: bool) -> (Vec<Event>, u64, bool) {
        let limit = limit.max(1);
        let slice: Vec<Event> = self
            .window
            .iter()
            .copied()
            .filter(|e| e.sequence >= from)
            .take(limit)
            .collect();
        let next = slice.last().map_or(from, |e| e.sequence + 1);
        if !fold {
            return (slice, next, self.window_dropped > 0);
        }
        let mut folder = RepeatFolder::new();
        let mut out = Vec::with_capacity(slice.len());
        for event in slice {
            folder.push(event, &mut |e| out.push(e));
        }
        folder.finish(&mut |e| out.push(e));
        (out, next, self.window_dropped > 0)
    }

    /// 开始录制轨迹。
    pub fn start_trace(&mut self, path: &Path, meta_json: &str, folding: bool) -> Result<()> {
        if self.recorder.is_some() {
            return Err(Error::Trace("该会话已经在录制中".into()));
        }
        self.recorder = Some(TraceRecorder::start(path, meta_json, folding)?);
        Ok(())
    }

    /// 是否正在录制。
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// 当前轨迹路径。
    #[must_use]
    pub fn trace_path(&self) -> Option<&Path> {
        self.recorder.as_ref().map(TraceRecorder::path)
    }

    /// 停止录制并拿到最终汇总。
    pub fn stop_trace(&mut self) -> Result<Summary> {
        let Some(mut recorder) = self.recorder.take() else {
            return Err(Error::Trace("该会话没有在录制".into()));
        };
        // 把通道丢弃数写进汇总：轨迹是否完整必须能在文件里看到
        recorder.note_dropped(self.channel_dropped());
        recorder.stop()
    }
}

impl Default for SessionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_start_at_one_and_are_monotonic() {
        let mut rt = SessionRuntime::with_capacity(4);
        let a = rt
            .record(EventKind::BreakpointHit, 7, 0x1000, &[(0, 0xAB)])
            .unwrap();
        let b = rt
            .record(EventKind::MemWrite, 7, 0x2000, &[(0, 4), (1, 0xDEAD)])
            .unwrap();
        assert_eq!((a.get(), b.get()), (1, 2));
        assert_eq!(rt.last_sequence(), 2);
        assert_eq!(rt.window()[0].arg(0), 0xAB);
    }

    #[test]
    fn window_is_bounded_and_reports_drops() {
        let mut rt = SessionRuntime::with_capacity(3);
        for i in 0..10 {
            rt.record(EventKind::StateChange, 1, 0x1000 + i, &[])
                .unwrap();
        }
        assert_eq!(rt.window().len(), 3);
        assert_eq!(rt.window_dropped(), 7);
        assert_eq!(rt.next_sequence_hint(), 10);
    }

    #[test]
    fn query_honours_from_and_limit() {
        let mut rt = SessionRuntime::with_capacity(64);
        for i in 0..20u64 {
            rt.record(EventKind::SyscallEnter, 1, 0x1000 + i, &[(0, 0x40)])
                .unwrap();
        }
        let (events, next, lossy) = rt.query(5, 3, false);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].sequence, 5);
        assert_eq!(next, 8);
        assert!(!lossy);
    }

    #[test]
    fn query_folds_identical_runs_in_the_window() {
        let mut rt = SessionRuntime::with_capacity(64);
        // 前 20 条载荷相同、序号连续 → 折叠成"代表 + 标记"
        for _ in 0..20 {
            rt.record(EventKind::SyscallEnter, 1, 0x2000, &[(0, 0x40)])
                .unwrap();
        }
        rt.record(EventKind::BreakpointHit, 9, 0x9000, &[]).unwrap();
        let (folded, next, _) = rt.query(1, 64, true);
        assert_eq!(folded.len(), 3, "20 条相同 + 1 条不同 → 折叠成 3 条记录");
        assert_eq!(folded[0].kind, EventKind::SyscallEnter);
        assert_eq!(folded[1].kind, EventKind::Repeat);
        assert_eq!(folded[1].arg(0), 19);
        assert_eq!(folded[2].kind, EventKind::BreakpointHit);
        assert_eq!(next, 22);

        let (raw, _, _) = rt.query(1, 64, false);
        assert_eq!(raw.len(), 21, "不折叠时应返回全部事件");
    }

    #[test]
    fn out_of_range_argument_index_is_rejected() {
        let mut rt = SessionRuntime::new();
        assert!(rt.record(EventKind::MemWrite, 1, 0, &[(99, 1)]).is_err());
    }

    #[test]
    fn double_start_of_trace_is_rejected() {
        let mut rt = SessionRuntime::new();
        let dir = std::env::temp_dir().join(format!("pb-rt-{}", std::process::id()));
        let path = dir.join("t.pbt");
        rt.start_trace(&path, "{}", true).unwrap();
        assert!(rt.is_recording());
        assert!(rt.trace_path().is_some());
        assert!(
            rt.start_trace(&path, "{}", true).is_err(),
            "重复开始录制必须报错"
        );
        let summary = rt.stop_trace().unwrap();
        assert_eq!(summary.records, 0);
        assert!(rt.stop_trace().is_err(), "未录制时停止必须报错");
        let _ = std::fs::remove_dir_all(dir);
    }
}
