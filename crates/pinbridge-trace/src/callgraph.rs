//! 调用图重建：从**转移事件**还原调用关系（见 ADR 0012）。
//!
//! 数据来源是工具侧「转移探针」发出的 `RegWrite` 事件（kind 8）：
//!
//! ```text
//! args[0] = 寄存器编号（0 = rip）
//! args[1] = 转移目标地址
//! args[2] = 转移码（1=call 2=ret；0 或缺省 = 未知）
//! ```
//!
//! **三个信号，缺一不建边**（这条是本模块的全部要点）：
//!
//! 1. 转移码必须明确（`1`/`2`）——`0`/缺省一律不建；
//! 2. 发起转移的指令地址（事件的 `address`）必须非零；
//! 3. 目标地址必须是**代码**：它要在同一份轨迹里作为某条事件的 `address`
//!    出现过。这条自洽性检查很关键——它挡掉的正是"把数据当代码"的错。
//!
//! 宁缺勿错：**一张漂亮但错的调用图会把后续所有推理带偏**，
//! 少一条边的代价只是"这里没看清"。所以本模块从不"猜"边，
//! 且会把每条未采纳的候选**按原因计数**报出来。

use std::collections::{BTreeMap, BTreeSet};

use crate::event::{Event, EventKind};

/// `args[0]` 里表示 RIP 的编号（与 `crate::REGISTER_NAMES[0]` 一致）。
pub const REGISTER_RIP: u64 = 0;
/// 转移码：调用。
pub const TRANSFER_CALL: u64 = 1;
/// 转移码：返回。
pub const TRANSFER_RET: u64 = 2;
/// `args[2]` 的槽位。
const ARG_TRANSFER_CODE: usize = 2;
/// `args[1]` 的槽位（目标地址）。
const ARG_TARGET: usize = 1;

/// 边的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// `call`：caller 在 `address`，callee 在目标。
    Call,
    /// `ret`：从当前函数返回。
    Return,
}

impl EdgeKind {
    /// 名字（报告与机器行都用它）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Return => "ret",
        }
    }
}

/// 一条边及其观测次数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    /// 边的种类。
    pub kind: EdgeKind,
    /// 发起方（调用指令所在地址）。
    pub from: u64,
    /// 目标（被调用方入口，或返回地址）。
    pub to: u64,
    /// 观测到的次数。只出现一次与出现上万次，含义完全不同。
    pub count: u64,
    /// 目标是否**在本轨迹里被验证为代码**（作为某条事件的地址出现过）。
    ///
    /// `false` 不等于"这条边是假的"：调用目标是未插装模块（系统 DLL）或本次运行
    /// 没执行到的地址时，我们**没有证据**说它是代码，也就没有证据反驳它。
    /// 所以保留边、明确标注，而不是丢掉它——丢掉会让"目标里调不到外部函数"
    /// 这种错结论看起来像事实。
    pub verified: bool,
}

/// 为什么某条候选**没有**被采纳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// 转移码未知（0 或缺省）：宁可当成"看不清"，也不猜。
    UnknownTransfer,
    /// 发起指令地址为 0：说不出"谁调用的"。
    NoSource,
    /// 目标不是代码（在本轨迹里从未作为任何事件的地址出现过）。
    ///
    /// **默认不启用**：它会把"调用未插装模块"这类完全正常的调用也拒掉。
    /// 只有显式要求"只要能在本轨迹里验证的边"时才用（`strict_code_targets`）。
    TargetNotCode,
    /// 自环：`from == to`，对调用关系没有信息量，且多半是探针落点问题。
    SelfLoop,
}

impl Rejection {
    /// 中文名（拒绝统计按它上报）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownTransfer => "转移码未知",
            Self::NoSource => "发起指令地址缺失",
            Self::TargetNotCode => "目标不是代码",
            Self::SelfLoop => "自环",
        }
    }
}

/// 调用图。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallGraph {
    /// 边的集合：`(kind, from, to)` → 次数。
    pub edges: BTreeMap<(EdgeKind, u64, u64), u64>,
    /// 图中出现过的地址（作为被调用方或发起方）。
    pub nodes: BTreeSet<u64>,
    /// 各拒绝原因的次数。
    pub rejections: BTreeMap<&'static str, u64>,
    /// `strict_code_targets` 模式下已验证过的边（用于统一分类）。
    verified_edges: BTreeSet<(EdgeKind, u64, u64)>,
    /// 输入里转移事件的总数（含被拒的）。
    pub candidates: u64,
    /// 输入里是否有折叠标记（折叠过的轨迹建图会缺边）。
    pub saw_folded_records: bool,
}

impl CallGraph {
    /// 调用边的数量。
    #[must_use]
    pub fn call_edge_count(&self) -> usize {
        self.edges
            .keys()
            .filter(|(k, _, _)| *k == EdgeKind::Call)
            .count()
    }

    /// 按次数排序的边（多者在前），取前 `limit` 条。
    #[must_use]
    pub fn top_edges(&self, limit: usize) -> Vec<Edge> {
        let verified = &self.verified_edges;
        let mut edges: Vec<Edge> = self
            .edges
            .iter()
            .map(|((kind, from, to), count)| Edge {
                kind: *kind,
                from: *from,
                to: *to,
                count: *count,
                verified: verified.contains(&(*kind, *from, *to)),
            })
            .collect();
        // 次数降序；同次数按地址升序，保证输出**稳定**（不然报告每次都不一样，
        // 人没法对比两份报告，自动化判据也没法写）。
        edges.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.from.cmp(&b.from))
                .then_with(|| a.to.cmp(&b.to))
        });
        edges.truncate(limit);
        edges
    }

    /// 有多少条调用边**有配对的返回**：返回地址正好落在调用指令之后
    /// （x86-64 上 `call rel32` 是 5 字节、`call r/m` 是 2 字节，
    /// 所以取 2..=7 这个窗口）。
    ///
    /// 这是本模块最有用的一个**自洽性指标**：调用图里最容易出的错是"边是错的
    /// 但看起来合理"，而"返回地址 = 调用指令 + 指令长度"这个等式不可能靠巧合
    /// 成立。配对数高，说明探针、事件编码、建图三段都对上了。
    ///
    /// 反向也成立：配对数**低**时不要相信这张图，先去看探针。
    #[must_use]
    pub fn paired_call_returns(&self) -> usize {
        // 判定只用一件事：**存在一条返回边的目标，正好落在调用指令之后
        // 2..=7 字节处**（`call rel32` 是 5 字节、`call r/m` 是 2 字节，
        // 短跳转形式另有几种长度，取这个窗口覆盖）。
        //
        // 不需要"目标已被验证为代码"：这条性质讲的是**返回地址**，
        // 与目标在不在本轨迹里出现过无关。第一版把它绑上 verified，
        // 于是只数到 4 条（真实值上百条）——判定条件多加一条，
        // 统计结果就少一大截，而报告不会告诉你少了什么。
        let returns: BTreeSet<u64> = self
            .edges
            .keys()
            .filter(|(kind, _, _)| *kind == EdgeKind::Return)
            .map(|(_, _, to)| *to)
            .collect();
        self.edges
            .keys()
            .filter(|(kind, from, _)| {
                *kind == EdgeKind::Call && (2..=7u64).any(|len| returns.contains(&(from + len)))
            })
            .count()
    }
    /// 机器可读的一行。
    #[must_use]
    pub fn machine_line(&self) -> String {
        format!(
            "PB_CALLS nodes={} call_edges={} paired={} all_edges={} candidates={} rejected={} folded={}",
            self.nodes.len(),
            self.call_edge_count(),
            self.paired_call_returns(),
            self.edges.len(),
            self.candidates,
            self.rejections.values().sum::<u64>(),
            u8::from(self.saw_folded_records),
        )
    }

    /// 报告。
    #[must_use]
    pub fn render(&self, limit: usize) -> String {
        let mut out = String::new();
        if self.saw_folded_records {
            out.push_str(
                "⚠ 输入里有折叠标记：折叠会吞掉中间的转移事件，**图会缺边**。\n\
                 \x20 要完整的调用关系，请先展开（`pinbridge replay` 的统计可以对照）。\n",
            );
        }
        if self.candidates == 0 {
            // 空图不能只说"空"：必须说清"为什么空"，否则调用方会以为目标没有调用。
            out.push_str(
                "这份轨迹里**没有转移事件**（`args[0]=0`(rip) 且 `args[2]` 为 1/2 的 reg_write）。\n\
                 \x20 原因通常是录制时没打开转移探针（`-pb_transfers 1`）——不是「目标没有调用」。\n",
            );
            out.push_str(&self.machine_line());
            out.push('\n');
            return out;
        }
        let unverified = self
            .edges
            .keys()
            .filter(|key| !self.verified_edges.contains(key))
            .count();
        out.push_str(&format!(
            "节点 {} 个，调用边 {} 条（全部边 {} 条），候选 {} 条\n",
            self.nodes.len(),
            self.call_edge_count(),
            self.edges.len(),
            self.candidates
        ));
        let paired = self.paired_call_returns();
        if paired != 0 {
            out.push_str(&format!(
                "  其中 {paired} 条调用边**有配对的返回**（返回地址 = 调用指令 + 指令长度）：\n  \
                 这个等式不可能靠巧合成立，配对数高说明探针→编码→建图三段都对上了。\n"
            ));
        }
        if unverified != 0 {
            // 说清"未验证"是什么意思：不是"假的"，是"我们没证据"。
            out.push_str(&format!(
                "  其中 {unverified} 条边的目标**未在本轨迹里出现**（未插装模块或未执行到）：\n  \
                 保留它们但不当作已验证——丢掉会让「目标里没有外部调用」这种错结论\n  \
                 看起来像事实。要只看已验证的边，用 `--strict-code-targets`。\n"
            ));
        }
        for (reason, count) in &self.rejections {
            out.push_str(&format!("  未采纳：{reason} {count} 条\n"));
        }
        if self.edges.is_empty() {
            out.push_str(
                "  **一条边都没有**：候选全被拒。这通常说明探针落点或转移码编码有问题，\n  \
                 而不是「目标真的没有调用关系」——别把空图当成结论。\n",
            );
        } else {
            out.push_str("次数最多的边：\n");
            for edge in self.top_edges(limit) {
                out.push_str(&format!(
                    "  {} {:#x} → {:#x} × {}\n",
                    edge.kind.as_str(),
                    edge.from,
                    edge.to,
                    edge.count
                ));
            }
        }
        out.push_str(&self.machine_line());
        out.push('\n');
        out
    }
}

/// 建图选项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallGraphOptions {
    /// 只要**能在本轨迹里验证目标为代码**的边；未验证的直接丢掉。
    ///
    /// 默认 **false**（保留并标注）。理由：调用目标是未插装模块时我们**没有证据**
    /// 说它是代码，但也没有证据反驳它；丢掉会让"目标里根本没有外部调用"这种
    /// 错结论看起来像事实。第一次真实运行的数据说明了这一点：默认严格时
    /// **19516 条调用候选全部被拒**，调用边变成 0——而这个目标显然在调函数。
    pub strict_code_targets: bool,
}

/// 从事件流建图。
///
/// 单遍扫描：先把"代码地址集合"建起来（所有事件的 `address`），
/// 再扫一遍转移事件。两遍而不是一遍，是因为**目标必须是代码**这条检查
/// 需要先知道全部代码地址——单遍会漏掉"目标在后面才被执行"的合法边。
#[must_use]
pub fn build(events: &[Event], options: CallGraphOptions) -> CallGraph {
    let mut graph = CallGraph::default();
    for event in events {
        if event.kind == EventKind::Repeat {
            graph.saw_folded_records = true;
        }
    }
    let code: BTreeSet<u64> = events.iter().map(|e| e.address).collect();

    for event in events {
        if event.kind != EventKind::RegWrite {
            continue;
        }
        // 只认 RIP —— 别的寄存器与调用关系无关。
        if event.args[0] != REGISTER_RIP {
            continue;
        }
        let code_of_transfer = event.args[ARG_TRANSFER_CODE];
        let target = event.args[ARG_TARGET];
        let source = event.address;
        // 只有明确标了 call/ret 的才算候选；未标的一律不算（连拒绝统计都不进，
        // 因为它们本来就不是"候选"——把未标的事件算进去会让拒绝率看起来很高）。
        let kind = match code_of_transfer {
            TRANSFER_CALL => EdgeKind::Call,
            TRANSFER_RET => EdgeKind::Return,
            _ => {
                // 但如果它**长得像**转移（args[1] 非零而转移码为 0），
                // 那说明有产出方在用老约定发事件——必须报出来，不能静默忽略。
                if target != 0 {
                    graph.candidates += 1;
                    *graph
                        .rejections
                        .entry(Rejection::UnknownTransfer.as_str())
                        .or_insert(0) += 1;
                }
                continue;
            }
        };
        graph.candidates += 1;
        if source == 0 {
            *graph
                .rejections
                .entry(Rejection::NoSource.as_str())
                .or_insert(0) += 1;
            continue;
        }
        if source == target {
            *graph
                .rejections
                .entry(Rejection::SelfLoop.as_str())
                .or_insert(0) += 1;
            continue;
        }
        let target_is_code = code.contains(&target);
        if !target_is_code && options.strict_code_targets {
            *graph
                .rejections
                .entry(Rejection::TargetNotCode.as_str())
                .or_insert(0) += 1;
            continue;
        }
        // **两种模式都要**记录"这条边被验证过没有"：只在严格模式下记，
        // 默认模式就会把所有边都标成未验证——标签错了比没有标签更糟。
        if target_is_code {
            graph.verified_edges.insert((kind, source, target));
        }
        *graph.edges.entry((kind, source, target)).or_insert(0) += 1;
        graph.nodes.insert(source);
        graph.nodes.insert(target);
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};

    /// 一条"转移事件"：`address` 是发起指令，args 按 ADR 0012 的约定。
    fn transfer(source: u64, target: u64, code: u64) -> Event {
        Event::new(1, EventKind::RegWrite, 7, source)
            .with_arg(0, REGISTER_RIP)
            .with_arg(1, target)
            .with_arg(ARG_TRANSFER_CODE, code)
    }

    /// 让某个地址"成为代码"：一条以它为地址的普通事件。
    fn code_at(address: u64) -> Event {
        Event::new(1, EventKind::BreakpointHit, 7, address)
    }

    #[test]
    fn a_complete_call_and_return_produce_both_edges() {
        // 返回地址 0x1005 也必须是代码：它就在 call 的**下一条指令**上，
        // 目标返回后必然执行到它。第一版数据里漏了它，返回边因此被
        // 「目标不是代码」正确拦下（规则按设计生效，是测试数据错了）。
        // 推论：轨迹若在返回**发生之前**就被截断，返回边会缺——
        // 这是规则的一部分，不是 bug。
        let events = vec![
            code_at(0x1000),
            code_at(0x1005),
            code_at(0x2000),
            transfer(0x1000, 0x2000, TRANSFER_CALL),
            transfer(0x2000, 0x1005, TRANSFER_RET),
        ];
        let graph = build(&events, CallGraphOptions::default());
        assert_eq!(graph.candidates, 2);
        assert_eq!(graph.call_edge_count(), 1);
        assert_eq!(graph.edges.len(), 2, "call 一条、ret 一条");
        let top = graph.top_edges(8);
        assert!(
            top.iter()
                .any(|e| e.kind == EdgeKind::Call && e.from == 0x1000 && e.to == 0x2000),
            "调用边: {top:?}"
        );
        assert!(
            top.iter()
                .any(|e| e.kind == EdgeKind::Return && e.from == 0x2000 && e.to == 0x1005),
            "返回边: {top:?}"
        );
        assert!(
            graph.rejections.is_empty(),
            "不该有拒绝: {:?}",
            graph.rejections
        );
    }

    /// 转移码未知 ⇒ **不建边**（宁可少画，不可画错）。
    ///
    /// 这条守的是"有产出方用老约定发事件"：`args[1]` 有目标但 `args[2]` 是 0。
    /// 静默忽略会让人以为"目标没有调用关系"，所以必须计数上报。
    #[test]
    fn an_unknown_transfer_code_is_rejected_and_counted() {
        let events = vec![
            code_at(0x1000),
            code_at(0x2000),
            transfer(0x1000, 0x2000, 0),
        ];
        let graph = build(&events, CallGraphOptions::default());
        assert!(graph.edges.is_empty(), "转移码未知不能建边");
        assert_eq!(graph.candidates, 1, "它算候选（长得像转移）");
        assert_eq!(
            graph.rejections.get(Rejection::UnknownTransfer.as_str()),
            Some(&1)
        );
        let text = graph.render(8);
        assert!(text.contains("转移码未知"), "要说清为什么没采纳: {text}");
    }

    /// 目标没在本轨迹里出现过 ⇒ **保留但标注未验证**（不是拒掉）。
    ///
    /// 这条是第一次真实运行纠正出来的：默认严格时，19516 条调用候选
    /// **全被拒**、调用边变成 0——而那个目标显然在调函数（它调了 ntdll）。
    /// 未插装模块里的目标我们**没有证据**说它是代码，也没有证据反驳它，
    /// 所以保留并标注；只有显式要求 `strict_code_targets` 时才丢。
    #[test]
    fn an_unseen_target_is_kept_but_marked_unverified() {
        let events = vec![
            code_at(0x1000),
            transfer(0x1000, 0xDEAD_BEEF, TRANSFER_CALL),
        ];
        let loose = build(&events, CallGraphOptions::default());
        assert_eq!(loose.edges.len(), 1, "边要保留");
        assert!(loose.rejections.is_empty());
        let edge = loose.top_edges(4).remove(0);
        assert!(!edge.verified, "但必须标注「未验证」");
        assert!(loose.render(4).contains("未在本轨迹里出现"), "报告要说清");

        let strict = build(
            &events,
            CallGraphOptions {
                strict_code_targets: true,
            },
        );
        assert!(strict.edges.is_empty(), "严格模式下才丢");
        assert_eq!(
            strict.rejections.get(Rejection::TargetNotCode.as_str()),
            Some(&1)
        );
    }

    /// 配对的 call/ret：返回地址 = 调用指令 + 指令长度（x86-64 `call rel32` = 5）。
    ///
    /// 这是"图对不对"最硬的一个信号：靠巧合凑不出这个等式。
    #[test]
    fn a_call_with_a_matching_return_is_counted_as_paired() {
        let events = vec![
            code_at(0x1000),
            code_at(0x1005),
            code_at(0x2000),
            code_at(0x2008),
            transfer(0x1000, 0x2000, TRANSFER_CALL),
            transfer(0x2008, 0x1005, TRANSFER_RET),
        ];
        let graph = build(&events, CallGraphOptions::default());
        assert_eq!(graph.paired_call_returns(), 1);
        assert!(graph.machine_line().contains("paired=1"));

        // 返回地址偏了 0x40 ⇒ 不配对（调用返回不到那里）
        let off_by = build(
            &[
                code_at(0x1000),
                code_at(0x3000),
                code_at(0x2000),
                code_at(0x2008),
                transfer(0x1000, 0x2000, TRANSFER_CALL),
                transfer(0x2008, 0x3040, TRANSFER_RET),
            ],
            CallGraphOptions::default(),
        );
        assert_eq!(off_by.paired_call_returns(), 0, "偏了就不算配对");
    }

    #[test]
    fn self_loops_and_missing_sources_are_rejected_with_reasons() {
        let events = vec![
            code_at(0x1000),
            transfer(0x1000, 0x1000, TRANSFER_CALL),
            transfer(0, 0x1000, TRANSFER_CALL),
        ];
        let graph = build(&events, CallGraphOptions::default());
        assert!(graph.edges.is_empty());
        assert_eq!(graph.rejections.get(Rejection::SelfLoop.as_str()), Some(&1));
        assert_eq!(graph.rejections.get(Rejection::NoSource.as_str()), Some(&1));
        let text = graph.render(8);
        assert!(text.contains("自环"), "{text}");
        assert!(text.contains("发起指令地址缺失"), "{text}");
    }

    /// 别的寄存器的写入与调用关系无关，**连候选都不算**。
    #[test]
    fn writes_to_other_registers_are_not_candidates() {
        let mut event = transfer(0x1000, 0x2000, TRANSFER_CALL);
        event.args[0] = 2; // rax
        let events = vec![code_at(0x1000), code_at(0x2000), event];
        let graph = build(&events, CallGraphOptions::default());
        assert_eq!(graph.candidates, 0);
    }

    /// 没有转移事件时，报告必须说**原因**，不能报一个看起来正常的空图。
    #[test]
    fn an_empty_graph_says_why_it_is_empty() {
        let events = vec![code_at(0x1000), code_at(0x2000)];
        let graph = build(&events, CallGraphOptions::default());
        let text = graph.render(8);
        assert!(text.contains("没有转移事件"), "{text}");
        assert!(text.contains("-pb_transfers"), "要指出怎么打开探针: {text}");
        assert!(
            text.contains("不是「目标没有调用」"),
            "别让空图被当成结论: {text}"
        );
        assert!(graph.machine_line().contains("candidates=0"));
    }

    #[test]
    fn folded_input_is_flagged_because_it_loses_edges() {
        let mut repeat = code_at(0x3000);
        repeat.kind = EventKind::Repeat;
        let events = vec![
            code_at(0x1000),
            code_at(0x2000),
            repeat,
            transfer(0x1000, 0x2000, TRANSFER_CALL),
        ];
        let graph = build(&events, CallGraphOptions::default());
        assert!(graph.saw_folded_records);
        let text = graph.render(8);
        assert!(text.contains("折叠"), "{text}");
        assert!(text.contains("图会缺边"), "要说清后果: {text}");
    }

    /// 排序必须稳定：次数相同的边按地址升序。否则同样的输入每次报告都不一样，
    /// 人没法对比两份报告，自动化判据也没法写。
    #[test]
    fn top_edges_are_deterministic() {
        let events = vec![
            code_at(0x1000),
            code_at(0x2000),
            code_at(0x3000),
            transfer(0x1000, 0x2000, TRANSFER_CALL),
            transfer(0x1000, 0x3000, TRANSFER_CALL),
            transfer(0x1000, 0x2000, TRANSFER_CALL),
        ];
        let graph = build(&events, CallGraphOptions::default());
        let first = graph.top_edges(8);
        let second = graph.top_edges(8);
        assert_eq!(first, second);
        assert_eq!(first[0].to, 0x2000, "次数多的在前");
        assert_eq!(first[0].count, 2);
        assert_eq!(first[1].to, 0x3000);
    }
}
