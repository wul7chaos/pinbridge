//! 两份轨迹的差分：**同一目标的两次运行，行为在哪里分岔**。
//!
//! 为什么这件事必须由框架来做，而不是让调用方自己 `diff` 两份导出文本：
//!
//! * 轨迹是二进制 delta 编码的，两次运行的地址、序号、参数都不同；
//! * 「不同」有很多种，含义完全不一样：
//!   - 走到不同的位置（`address` 不同）⇒ **控制流**分岔；
//!   - 同一位置做了不同的事（`args` 不同）⇒ **数据**分岔；
//!   - 事件类型都不同（`kind` 不同）⇒ 执行到了完全不同的代码路径；
//!
//!   只说「第 42 条不一样」对分析没有帮助，**说清是哪一类**才有。
//! * 最危险的情况是**轨迹本身不全**（录制时丢过事件）：这时任何「两次不一样」
//!   的结论都可能是丢数据造成的假象。所以这里**主动检查并告警**，
//!   而不是等调用方自己想起来看汇总。
//!
//! 纯离线计算，不碰目标、不需要 Pin。

use crate::event::Event;
use crate::pbt::Summary;

/// 差分选项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// 忽略 `address` 字段。
    ///
    /// 跨运行对比**几乎总要用它**：ASLR 让同一个函数在两次运行里地址不同，
    /// 不忽略的话第一处分岔永远是因为基址不同——那是噪音，会把真正的
    /// 行为差异埋掉。默认 **false**（不忽略）：默认值必须"所见即所得"，
    /// 需要归一化的时候显式打开，而不是让人以为比的是全部字段。
    pub ignore_address: bool,
    /// 忽略 `thread_id`。目标里线程调度不确定时有用。
    pub ignore_thread: bool,
}

/// 一处分岔。`sequence` 用**左侧**（基准）的序号：两份轨迹的序号各自从 1 起，
/// 位置对齐后比较，所以报出来的序号是"基准轨迹里的第几条"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Difference {
    /// 基准轨迹里的序号（1 起）。
    pub sequence: u64,
    /// 记录在两侧的**下标**（0 起）。序号可能因丢弃而跳号，
    /// 下标才是「对齐位置」本身。
    pub index: usize,
    /// 第一个不同的字段名：`kind` / `thread` / `address` / `arg1`…`arg8`。
    pub field: &'static str,
    /// 左侧值。
    pub left: u64,
    /// 右侧值。
    pub right: u64,
}

/// 差分的分类。**这个分类是给人/AI 直接下的结论**，不是原始数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// 两份完全一致（长度与每条记录都相同）。
    Identical,
    /// 一份是另一份的前缀：跑到后面另一份还有事件，或者反过来。
    Prefix,
    /// 同一位置上执行到了不同的地方（`kind` 或 `address` 不同）⇒ 控制流分岔。
    ControlFlow,
    /// 同一位置上事件类型与地址都相同，但参数不同 ⇒ 数据分岔。
    Data,
    /// 事件类型不同 ⇒ 走了完全不同的代码路径。
    Kind,
}

impl Divergence {
    /// 中文名，用于报告里直接给人/AI 看。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identical => "一致",
            Self::Prefix => "前缀",
            Self::ControlFlow => "控制流分岔",
            Self::Data => "数据分岔",
            Self::Kind => "类型不同",
        }
    }

    /// 这条结论能不能用来下判断。
    ///
    /// `Prefix` 与 `Identical` 是「没发现分岔」，另外三种是「发现了」。
    #[must_use]
    pub const fn is_divergent(self) -> bool {
        matches!(self, Self::ControlFlow | Self::Data | Self::Kind)
    }
}

/// 差分结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceDiff {
    /// 左侧（基准）的记录条数。
    pub left_records: usize,
    /// 右侧（对照）的记录条数。
    pub right_records: usize,
    /// 左侧最后一条记录的序号（轨迹为空时为 0）。
    pub left_last_sequence: u64,
    /// 右侧最后一条记录的序号（轨迹为空时为 0）。
    pub right_last_sequence: u64,
    /// 两份都同意的前缀**长度**（记录条数）。
    pub common_prefix: usize,
    /// 分类。
    pub divergence: Divergence,
    /// 前若干处分岔（按位置升序）。不是全量——全量在几十万条上没人看。
    pub differences: Vec<Difference>,
    /// 分岔总数（可能大于 `differences.len()`）。
    pub difference_count: usize,
    /// **必须先看的告警**：轨迹本身不完整时，上面所有结论都不可靠。
    pub warnings: Vec<String>,
}

impl TraceDiff {
    /// 是否有告警（有告警时**先看告警**，再看结论）。
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// 分岔发生在「第几条之后」（0 表示从第一条就不一样）。
    #[must_use]
    pub const fn agrees_up_to(&self) -> u64 {
        self.common_prefix as u64
    }

    /// 机器可读的一行摘要。
    ///
    /// 为什么单独给一行：自动化判据不该去解析给人看的散文。
    /// 这一行是稳定的键值对，测试与调用方都按它判定。
    #[must_use]
    pub fn machine_line(&self) -> String {
        let first = self
            .differences
            .first()
            .map_or_else(|| "-".to_owned(), |d| d.sequence.to_string());
        format!(
            "PB_DIFF divergence={} agrees_up_to={} left={} right={} first_sequence={} differences={} warnings={}",
            self.divergence.as_str(),
            self.agrees_up_to(),
            self.left_records,
            self.right_records,
            first,
            self.difference_count,
            self.warnings.len(),
        )
    }

    /// 给人/AI 看的报告。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for warning in &self.warnings {
            out.push_str(&format!("⚠ {warning}\n"));
        }
        out.push_str(&format!(
            "两侧记录数：{} / {}，最后序号：{} / {}\n",
            self.left_records,
            self.right_records,
            self.left_last_sequence,
            self.right_last_sequence
        ));
        out.push_str(&format!(
            "共同前缀：{} 条（第 {} 条之后开始分岔）\n",
            self.common_prefix,
            self.agrees_up_to()
        ));
        out.push_str(&format!("结论：{}\n", self.divergence.as_str()));
        match self.divergence {
            Divergence::Identical => out.push_str("  两次运行的记录逐条相同。\n"),
            Divergence::Prefix => out.push_str(
                "  一侧跑得比另一侧久或早退；共同前缀内**没有分岔**——所以「行为不同」\n\
                 \x20 这件事在这份数据上得不到支持，别硬下结论。\n",
            ),
            Divergence::ControlFlow => out.push_str(
                "  同一位置上执行到了不同的地方 ⇒ 控制流分岔（条件/分支/异常路径不同）。\n",
            ),
            Divergence::Data => out.push_str(
                "  同一位置做了不同的事 ⇒ 数据分岔（输入、时间、随机数、堆布局不同）。\n",
            ),
            Divergence::Kind => out.push_str("  事件类型不同 ⇒ 走了完全不同的代码路径。\n"),
        }
        if !self.differences.is_empty() {
            out.push_str("前几处分岔：\n");
            for d in &self.differences {
                out.push_str(&format!(
                    "  #{} 序号 {} 字段 {}：{:#x} → {:#x}\n",
                    d.index, d.sequence, d.field, d.left, d.right
                ));
            }
            if self.difference_count > self.differences.len() {
                out.push_str(&format!(
                    "  （共 {} 处分岔，这里只列了前 {} 处）\n",
                    self.difference_count,
                    self.differences.len()
                ));
            }
        }
        out.push_str(&self.machine_line());
        out.push('\n');
        out
    }
}

/// 记录的原始可比字段：`(kind, thread_id, address, args, argc)`。
///
/// 单独抽出来是为了让「比较」这件事只有一处实现——差分与测试都走它，
/// 免得两处规则不一致（那种不一致极难发现：两边各有一套「相等」的定义）。
fn fields_of(event: &Event) -> (u8, u32, u64, [u64; crate::event::MAX_ARGS]) {
    (event.kind as u8, event.thread_id, event.address, event.args)
}

/// 比较两份轨迹。
///
/// `max_differences` 是**保留**的分岔条数（不是总数），避免一条噪声很大的轨迹
/// 把报告撑爆——每处分岔都要占内存，而调用方真正需要的是「第一处」与「有多少处」。
#[must_use]
pub fn diff_traces(
    left: &[Event],
    right: &[Event],
    options: DiffOptions,
    max_differences: usize,
) -> TraceDiff {
    let common = left.len().min(right.len());
    let mut common_prefix = 0usize;
    let mut differences: Vec<Difference> = Vec::new();
    let mut difference_count = 0usize;
    let mut first_class: Option<Divergence> = None;

    for index in 0..common {
        let (lk, lt, la, largs) = fields_of(&left[index]);
        let (rk, rt, ra, rargs) = fields_of(&right[index]);
        // 字段优先级：kind → address → thread → args。
        // 顺序是有讲究的：先判"是不是同一类事件、同一个位置"，
        // 最后才比参数。反过来会把"控制流分岔"报成"数据分岔"——
        // 而这两者对应完全不同的排查方向。
        let mut found: Option<Difference> = None;
        if lk != rk {
            found = Some(Difference {
                sequence: left[index].sequence,
                index,
                field: "kind",
                left: u64::from(lk),
                right: u64::from(rk),
            });
        } else if !options.ignore_address && la != ra {
            found = Some(Difference {
                sequence: left[index].sequence,
                index,
                field: "address",
                left: la,
                right: ra,
            });
        } else if !options.ignore_thread && lt != rt {
            found = Some(Difference {
                sequence: left[index].sequence,
                index,
                field: "thread",
                left: u64::from(lt),
                right: u64::from(rt),
            });
        } else {
            for slot in 0..crate::event::MAX_ARGS {
                if largs[slot] != rargs[slot] {
                    found = Some(Difference {
                        sequence: left[index].sequence,
                        index,
                        field: ARG_FIELDS[slot],
                        left: largs[slot],
                        right: rargs[slot],
                    });
                    break;
                }
            }
        }
        if let Some(difference) = found {
            if first_class.is_none() {
                // 第一处分岔决定分类。后面的分岔大多是它的**后果**——
                // 一次控制流分岔之后，两条轨迹本来就会一路不同。
                first_class = Some(match difference.field {
                    "kind" => Divergence::Kind,
                    "address" => Divergence::ControlFlow,
                    _ => Divergence::Data,
                });
            }
            difference_count += 1;
            if differences.len() < max_differences {
                differences.push(difference);
            }
        } else if first_class.is_none() {
            common_prefix = index + 1;
        }
    }

    let divergence = match first_class {
        Some(class) => class,
        None => {
            if left.len() == right.len() {
                Divergence::Identical
            } else {
                Divergence::Prefix
            }
        }
    };

    TraceDiff {
        left_records: left.len(),
        right_records: right.len(),
        left_last_sequence: left.last().map_or(0, |e| e.sequence),
        right_last_sequence: right.last().map_or(0, |e| e.sequence),
        common_prefix,
        divergence,
        differences,
        difference_count,
        warnings: Vec::new(),
    }
}

const ARG_FIELDS: [&str; crate::event::MAX_ARGS] = [
    "arg1", "arg2", "arg3", "arg4", "arg5", "arg6", "arg7", "arg8",
];

/// 检查轨迹汇总里「这份数据能不能用来比对」，返回告警。
///
/// 单独一个函数是因为它**必须**在差分之前被想到。判决型的结论建立在
/// 不完整的数据上，比没有结论更糟：调用方会照着它改代码。
#[must_use]
pub fn diff_warnings(left: Option<&Summary>, right: Option<&Summary>) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut check = |side: &str, summary: Option<&Summary>| {
        let Some(summary) = summary else {
            warnings.push(format!(
                "{side} 侧没有汇总（SUMMARY）：这份轨迹可能只有提交标记或根本没写完，\
                 逐条比对的结果不可作为结论"
            ));
            return;
        };
        if summary.dropped != 0 {
            warnings.push(format!(
                "{side} 侧录制时丢弃了 {} 条事件：两侧的分岔可能只是**丢数据**造成的假象",
                summary.dropped
            ));
        }
        if summary.truncated {
            warnings.push(format!("{side} 侧录制被异常中断：尾部不完整"));
        }
    };
    check("左", left);
    check("右", right);
    if let (Some(l), Some(r)) = (left, right) {
        if l.repeat_folded != 0 || r.repeat_folded != 0 {
            warnings.push(format!(
                "存在重复折叠（左 {} 条 / 右 {} 条被折叠）：折叠对时序敏感，\
                 **比对用的必须是展开后的数据**（`pinbridge_trace::fold::expand_records`）；\
                 拿带折叠标记的记录直接比，会把折叠位置的差异变成大量伪分岔",
                l.repeat_folded, r.repeat_folded
            ));
        }
    }
    warnings
}

/// 把错过的分岔数补进结果里（`diff_traces` 之后调用，保持纯函数结构）。
pub fn attach_warnings(diff: &mut TraceDiff, warnings: Vec<String>) {
    diff.warnings = warnings;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};

    fn event(sequence: u64, kind: EventKind, address: u64, args: &[u64]) -> Event {
        let mut e = Event::new(sequence, kind, 7, address);
        for (slot, value) in args.iter().enumerate() {
            e = e.with_arg(slot, *value);
        }
        e
    }

    fn base() -> Vec<Event> {
        vec![
            event(1, EventKind::SyscallEnter, 0x1400_0010, &[0x40, 0x1000]),
            event(2, EventKind::MemWrite, 0x1400_0020, &[0x2000, 4, 0xAA]),
            event(3, EventKind::SyscallExit, 0x1400_0010, &[0x40, 0]),
        ]
    }

    #[test]
    fn identical_traces_are_identical() {
        let diff = diff_traces(&base(), &base(), DiffOptions::default(), 8);
        assert_eq!(diff.divergence, Divergence::Identical);
        assert_eq!(diff.agrees_up_to(), 3, "全部 3 条都一致");
        assert_eq!(diff.difference_count, 0);
        assert!(!diff.divergence.is_divergent());
        assert!(diff.machine_line().contains("divergence=一致"));
    }

    /// **先分类，再报位置**：类型不同 ⇒ 走了完全不同的代码路径。
    #[test]
    fn a_kind_difference_is_reported_as_its_own_class() {
        let mut right = base();
        right[2] = event(3, EventKind::MemRead, 0x1400_0010, &[0x40, 0]);
        let diff = diff_traces(&base(), &right, DiffOptions::default(), 8);
        assert_eq!(diff.divergence, Divergence::Kind);
        assert_eq!(diff.agrees_up_to(), 2, "第 2 条之后开始分岔");
        let first = diff.differences.first().expect("应当有一处分岔");
        assert_eq!(first.sequence, 3);
        assert_eq!(first.index, 2);
        assert_eq!(first.field, "kind", "类型字段优先于地址与参数");
    }

    /// 同一位置做了不同的事 ⇒ 数据分岔，且**报出是哪个参数**。
    #[test]
    fn a_parameter_difference_is_reported_as_data_and_names_the_argument() {
        let mut right = base();
        right[1] = event(2, EventKind::MemWrite, 0x1400_0020, &[0x2000, 4, 0xBB]);
        let diff = diff_traces(&base(), &right, DiffOptions::default(), 8);
        assert_eq!(diff.divergence, Divergence::Data);
        let first = diff.differences.first().expect("应当有一处分岔");
        assert_eq!(first.field, "arg3", "参数按 1 起编号，第三个参数是 arg3");
        assert_eq!(first.left, 0xAA);
        assert_eq!(first.right, 0xBB);
    }

    /// **ASLR 是这类对比里最大的噪声源**：地址每次运行都不同。
    #[test]
    fn ignoring_addresses_turns_aslr_noise_into_a_clean_comparison() {
        let mut right = base();
        for (i, e) in right.iter_mut().enumerate() {
            e.address = 0x7F00_0000 + (i as u64) * 0x1000;
        }
        // 不忽略：第一处分岔就是地址（真实，但对"行为有没有不同"是噪声）
        let noisy = diff_traces(&base(), &right, DiffOptions::default(), 8);
        assert_eq!(noisy.divergence, Divergence::ControlFlow);
        assert_eq!(noisy.agrees_up_to(), 0);
        // 忽略地址：类型与参数完全一致 ⇒ 行为一致
        let clean = diff_traces(
            &base(),
            &right,
            DiffOptions {
                ignore_address: true,
                ..DiffOptions::default()
            },
            8,
        );
        assert_eq!(clean.divergence, Divergence::Identical);
        assert_eq!(clean.difference_count, 0);
    }

    /// 一份比另一份短：**没有分岔**不等于"行为不同"。
    #[test]
    fn a_prefix_is_not_a_divergence_and_says_so() {
        let left = base();
        let right = base()[..2].to_vec();
        let diff = diff_traces(&left, &right, DiffOptions::default(), 8);
        assert_eq!(diff.divergence, Divergence::Prefix);
        assert_eq!(diff.agrees_up_to(), 2);
        assert!(!diff.divergence.is_divergent(), "前缀不是分岔");
        let text = diff.render();
        assert!(
            text.contains("得不到支持"),
            "必须说清这条结论的边界: {text}"
        );
    }

    #[test]
    fn difference_count_is_exact_even_when_only_a_few_are_kept() {
        let left = base();
        let mut right = base();
        for (i, e) in right.iter_mut().enumerate() {
            e.address = 0x9000 + i as u64;
        }
        let diff = diff_traces(&left, &right, DiffOptions::default(), 1);
        assert_eq!(diff.difference_count, 3, "总数要准确");
        assert_eq!(diff.differences.len(), 1, "保留条数受上限约束");
        assert!(diff.render().contains("共 3 处分岔"));
    }

    /// 轨迹本身不可信时，**结论必须先作废**。
    #[test]
    fn an_incomplete_trace_produces_a_warning_that_says_the_conclusion_is_unsound() {
        let mut left_summary = Summary {
            records: 3,
            last_sequence: 3,
            ..Summary::default()
        };
        left_summary.dropped = 17;
        let right_summary = Summary {
            records: 3,
            last_sequence: 3,
            repeat_folded: 5,
            ..Summary::default()
        };
        let warnings = diff_warnings(Some(&left_summary), Some(&right_summary));
        assert!(
            warnings.iter().any(|w| w.contains("丢弃了 17 条")),
            "丢数据必须告警: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("假象")),
            "要说清后果是「分岔可能是假象」: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("折叠")),
            "折叠对时序敏感，必须提醒先展开: {warnings:?}"
        );
        // 没有汇总也必须说，而不是当成"没问题"
        let missing = diff_warnings(None, Some(&right_summary));
        assert!(
            missing.iter().any(|w| w.contains("没有汇总")),
            "缺汇总要明说: {missing:?}"
        );
    }

    #[test]
    fn empty_traces_are_identical_and_have_no_warnings_of_their_own() {
        let diff = diff_traces(&[], &[], DiffOptions::default(), 8);
        assert_eq!(diff.divergence, Divergence::Identical);
        assert_eq!(diff.left_last_sequence, 0);
        assert!(diff.machine_line().contains("agrees_up_to=0"));
    }
}
