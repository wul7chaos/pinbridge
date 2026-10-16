//! 污点传播：回答「这些字节是从哪复制来的」。
//!
//! 与反向切片（[`crate::slice`]）的分工：切片是**地址级**的，它能说清「谁写过、
//! 谁读过、何时执行」；本模块处理**数据流**，回答「这几个字节的候选来源」。
//!
//! 规则与验收见 `docs/adr/0014-taint-granularity-and-its-acceptance.md`。有两条
//! 硬约束直接写在这里，因为它们是这个模块存在的意义：
//!
//! 1. **值的相等是候选关系，不是因果关系。** 动态插桩看不到 CPU 的意图，
//!    相等的值可能只是巧合。所以报告用词永远是「候选来源」，绝不写「来自」。
//! 2. **追溯不到与没有污点是两件事。** 录像开始前就在内存里的字节根本没有来源，
//!    它们进 `unresolved`，而不是被当成"干净"。
//!
//! 两个粒度（[`Granularity::Region`] 与 [`Granularity::Byte`]）共用同一遍回放，
//! 只有匹配那一步不同，所以「近似版本是字节级的超集」这件事是**构造性成立**的，
//! 不依赖约定——验收要的「误报显著减少」因此可以被测出来。

use std::collections::HashMap;

use crate::event::{Event, EventKind};
use crate::slice::CoverageGap;

/// 传播粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Granularity {
    /// 结构化近似：只要目标区域内**有任何**字节匹配上某次读取的区域，
    /// 就把**整个**区域算作来源于那次读取。
    ///
    /// 这是手写脚本时最容易写出来的那一版（拿"这块被读过""这块被写过"做区间
    /// 交叠），保留它不是为了好看，而是为了**给字节级版本一个可比较的基线**。
    Region,
    /// 字节级：逐字节匹配，只有值相等的那个字节才挂上候选来源。
    #[default]
    Byte,
}

impl Granularity {
    /// 短名（报告与命令行共用）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Byte => "byte",
        }
    }

    /// 从命令行文本解析。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "region" => Some(Self::Region),
            "byte" => Some(Self::Byte),
            _ => None,
        }
    }
}

/// 一个候选来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaintOrigin {
    /// 那次读取的地址（候选来源区域的起点）。
    pub address: u64,
    /// 那次读取的长度。
    pub len: u32,
    /// 那次读取的序号（同一个地址被多次读取时用来区分"哪一次"）。
    pub sequence: u64,
}

/// 单个字节的追溯结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteTaint {
    /// 字节地址。
    pub address: u64,
    /// 候选来源（可能不止一个：同值多源时无法区分，只能并列）。
    pub origins: Vec<TaintOrigin>,
    /// 是否**追溯不到**（本轨迹里没有任何可匹配的来源）。
    ///
    /// 注意它不等于"这一格没有污点"：录像开始前就存在的字节也落在这里。
    pub unresolved: bool,
}

/// 污点传播报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintReport {
    /// 目标范围起点。
    pub target: u64,
    /// 目标范围长度。
    pub len: u64,
    /// 本次使用的粒度。
    pub granularity: Granularity,
    /// 逐字节结果（按地址升序）。
    pub bytes: Vec<ByteTaint>,
    /// 候选总数（逐字节累加）。**这是误报的度量**：近似粒度会更大。
    pub candidates: usize,
    /// 恰好只有一个候选来源的字节数。
    pub exact_bytes: usize,
    /// 追溯不到的字节数。
    pub unresolved: usize,
    /// 轨迹自带的覆盖缺口（有缺口时结论只能当作下界）。
    pub coverage_gap: Option<CoverageGap>,
}

impl TaintReport {
    /// 机器可读的一行（供脚本与回归对比）。
    #[must_use]
    pub fn machine_line(&self) -> String {
        format!(
            "PB_TAINT target={:#x} len={} granularity={} candidates={} exact={} unresolved={}",
            self.target,
            self.len,
            self.granularity.name(),
            self.candidates,
            self.exact_bytes,
            self.unresolved
        )
    }

    /// 人读的报告。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "目标范围 {:#x}..{:#x}（{} 字节），粒度 {}。\n",
            self.target,
            self.target.saturating_add(self.len),
            self.len,
            self.granularity.name()
        ));
        // ── 缺口优先说：它决定下面的结论是"下界"还是"完整" ──
        if let Some(gap) = self.coverage_gap {
            if gap.capped {
                out.push_str(
                    "⚠ 录制时**探针达到事件上限并停止采集**：缺失的事件里可能有真正的来源，\n\
                     \x20 所以下面的候选集合只能当作**下界**（至少这些），不能当作完整答案。\n",
                );
            }
            if gap.skipped != 0 || gap.unreadable != 0 {
                out.push_str(&format!(
                    "⚠ 录制时存在采集缺口（跳过写入点 {} 个、读不出 {} 条）：\n\
                     \x20 「追溯不到」可能只是缺口造成的，不一定说明录像前就存在。\n",
                    gap.skipped, gap.unreadable
                ));
            }
        }
        out.push_str(&format!(
            "候选来源 {} 个（逐字节累加）、唯一来源 {} 字节、追溯不到 {} 字节。\n",
            self.candidates, self.exact_bytes, self.unresolved
        ));
        // 逐字节明细：只列有候选或明确追溯不到的，且限条数
        let mut shown = 0usize;
        for byte in &self.bytes {
            if byte.origins.is_empty() {
                continue;
            }
            if shown >= 12 {
                out.push_str("  …（其余省略）\n");
                break;
            }
            let list: Vec<String> = byte
                .origins
                .iter()
                .map(|origin| {
                    format!(
                        "{:#x}(+{}B, seq {})",
                        origin.address, origin.len, origin.sequence
                    )
                })
                .collect();
            out.push_str(&format!(
                "  {:#x} → 候选 {} 个：{}\n",
                byte.address,
                byte.origins.len(),
                list.join("、")
            ));
            shown += 1;
        }
        if self.unresolved != 0 {
            out.push_str(
                "追溯不到的含义：本轨迹里没有任何可匹配的来源（录像开始前就在内存里、\n\
                 \x20 来自未采集的路径、或者根本不是复制来的）。它**不是**「没有污点」。\n",
            );
        }
        out.push_str(
            "说明：值的相等是**候选**关系，不是因果——动态插桩看不到 CPU 的意图。\n\
             \x20 本模块不做过程间分析，也不处理被编译器优化掉的中间值；\n\
             \x20 批量复制只能靠逐字节值匹配认出，同值多源时只能并列候选。\n",
        );
        out
    }
}

/// 回放事件流，对目标范围做污点传播。
///
/// 匹配规则（ADR 0014）：把写入的字节序列拿去和**此前读到的字节序列**做
/// 子序列匹配——按**值**相等，不是按地址相等。复制意味着"从 S 读到的东西出现在
/// T"，所以拿地址去比是错的（这一步在本模块的第一版里就写错了一次，用例把它抓住了）。
///
/// 两个粒度共用这一遍回放，差别只在命中之后铺开的范围：字节级只铺真正匹配上的
/// 那些字节，近似粒度只要**有**匹配就把整块写入都算上。
#[must_use]
pub fn propagate(events: &[Event], target: u64, len: u64, granularity: Granularity) -> TaintReport {
    /// 一次读取：起点、长度、值、序号。
    struct ReadRecord {
        address: u64,
        size: u32,
        value: u64,
        sequence: u64,
    }
    // 只保留最近若干次读取：值匹配是 O(读取数 × 8)，不设界会让长轨迹变成
    // 平方级。超出窗口的读取仍可能才是真正的来源，所以报告里要能看出这一点
    // （进 `unresolved`，并在这里留个明确的注释而不是悄悄丢）。
    const READ_WINDOW: usize = 8192;
    let mut reads: Vec<ReadRecord> = Vec::new();
    let mut taint: HashMap<u64, Vec<TaintOrigin>> = HashMap::new();

    let in_target = |address: u64| address >= target && address < target.saturating_add(len);

    for event in events {
        match event.kind {
            EventKind::MemRead => {
                let size = event.args[1] as u32;
                if size == 0 {
                    continue;
                }
                reads.push(ReadRecord {
                    address: event.args[0],
                    size,
                    value: event.args[2],
                    sequence: event.sequence,
                });
                if reads.len() > READ_WINDOW {
                    reads.remove(0);
                }
            }
            EventKind::MemWrite => {
                let base = event.args[0];
                let size = event.args[1] as u32;
                let value = event.args[2];
                if size == 0 {
                    continue;
                }
                // 值只有 8 字节：比它长的写入拿不到值，也就无从匹配。
                // 这里**不猜**（不假设高字节为 0），直接跳过匹配。
                let valued = size.min(8);
                // 逐字节找候选：在每一次读取的值里找"写入的前若干字节"。
                // 计数一律用 u32：两边都是"字节数"，混用 usize/u32 只会带来
                // 转换噪音与真实的移位溢出风险。
                // 按**偏移去重**：一段连续匹配会被它覆盖的每个偏移各命中一次，
                // 用 Vec 会重复计数（第一版就是这样：4+3+2+1=10），
                // 而"候选总数"是验收指标，重复计数会直接把指标弄坏。
                let mut hits: HashMap<u32, TaintOrigin> = HashMap::new();
                'outer: for offset in 0..valued {
                    let want = ((value >> (8 * offset)) & 0xFF) as u8;
                    // 从最近的读取往回找：越近越可能是真正的来源。
                    for read in reads.iter().rev() {
                        let read_bytes = read.size.min(8);
                        for inner in 0..read_bytes {
                            let got = ((read.value >> (8 * inner)) & 0xFF) as u8;
                            if got != want {
                                continue;
                            }
                            // 找到即**再确认后面几个字节**是否连续一致，
                            // 一致就把它们一起认下来（比单字节匹配精确得多）。
                            let mut run = 1u32;
                            while offset + run < valued && inner + run < read_bytes {
                                let w = ((value >> (8 * (offset + run))) & 0xFF) as u8;
                                let g = ((read.value >> (8 * (inner + run))) & 0xFF) as u8;
                                if w != g {
                                    break;
                                }
                                run += 1;
                            }
                            let origin = TaintOrigin {
                                address: read.address.wrapping_add(u64::from(inner)),
                                len: run,
                                sequence: read.sequence,
                            };
                            for step in 0..run {
                                hits.entry(offset + step).or_insert(origin);
                            }
                            // 一个字节只认第一个（最近的）来源作为起点；
                            // 同值多源的情况在 `unresolved` 的说明里已经交代。
                            continue 'outer;
                        }
                    }
                }

                let spread: Vec<(u64, TaintOrigin)> = match granularity {
                    // 字节级：只挂真正匹配上的字节。
                    Granularity::Byte => hits
                        .iter()
                        .map(|(offset, origin)| (base.wrapping_add(u64::from(*offset)), *origin))
                        .collect(),
                    // 近似：只要**有**匹配，就把整块写入都算作来源于第一个命中的读取。
                    // 这一行就是"误报"的来源，也是 ADR 0014 要对比的东西。
                    Granularity::Region => match hits.iter().min_by_key(|(offset, _)| **offset) {
                        // 取偏移最小的那个命中作为"整块的来源"：近似粒度不区分
                        // 字节，所以只能挑一个代表——这正是它误报的来源。
                        Some((_, first)) => (0..u64::from(size))
                            .map(|offset| (base.wrapping_add(offset), *first))
                            .collect(),
                        None => Vec::new(),
                    },
                };
                for (address, origin) in spread {
                    if !in_target(address) {
                        continue;
                    }
                    let slot = taint.entry(address).or_default();
                    if !slot.contains(&origin) {
                        slot.push(origin);
                    }
                }
                // 写入**不**登记"读到过的值"：否则传播会自证（自己写、自己认）。
            }
            _ => {}
        }
    }

    let mut bytes = Vec::new();
    let mut candidates = 0usize;
    let mut exact = 0usize;
    let mut unresolved = 0usize;
    for offset in 0..len {
        let address = target.saturating_add(offset);
        let origins = taint.remove(&address).unwrap_or_default();
        candidates += origins.len();
        if origins.len() == 1 {
            exact += 1;
        }
        let is_unresolved = origins.is_empty();
        if is_unresolved {
            unresolved += 1;
        }
        bytes.push(ByteTaint {
            address,
            origins,
            unresolved: is_unresolved,
        });
    }

    TaintReport {
        target,
        len,
        granularity,
        bytes,
        candidates,
        exact_bytes: exact,
        unresolved,
        coverage_gap: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(sequence: u64, address: u64, size: u64, value: u64) -> Event {
        Event::new(sequence, EventKind::MemRead, 7, 0x1000)
            .with_arg(0, address)
            .with_arg(1, size)
            .with_arg(2, value)
    }

    fn write(sequence: u64, address: u64, size: u64, value: u64) -> Event {
        Event::new(sequence, EventKind::MemWrite, 7, 0x2000)
            .with_arg(0, address)
            .with_arg(1, size)
            .with_arg(2, value)
    }

    /// 验收场景：16 字节的源区域里只有 4 字节被复制过去。
    ///
    /// 这条用例就是 ADR 0014 里写下的判据本身：**近似粒度必然多报，字节级只报真的**。
    /// 两个粒度跑同一份事件流，所以差别只可能来自匹配那一步。
    #[test]
    fn byte_granularity_reports_fewer_candidates_than_the_region_approximation() {
        // 一次 8 字节的写入里**只有低 4 字节**来自源区域 —— 这就是"部分复制"的
        // 真实形状：动态插桩只看得见"写入了这 8 字节"，看不出其中哪几个字节是搬来的。
        let source = 0x5000u64;
        let target = 0x6000u64;
        let source_value: u64 = 0x8877_6655_4433_2211;
        let copied: u64 = source_value & 0xFFFF_FFFF; // 低 4 字节 = 0x4433_2211
                                                      // 高 4 字节是本地算出来的，与源区域无关
        let local: u64 = 0x0000_0000_AABB_CCDD;
        let written: u64 = copied | (local << 32);
        let events = vec![
            read(1, source, 8, source_value),
            write(2, target, 8, written),
        ];

        let bytes = propagate(&events, target, 8, Granularity::Byte);
        let region = propagate(&events, target, 8, Granularity::Region);

        assert_eq!(bytes.len, 8);
        assert_eq!(bytes.granularity, Granularity::Byte);
        // 字节级：只有真正来自源的 4 字节挂上候选
        assert_eq!(bytes.candidates, 4, "字节级只应挂上被真正搬来的 4 字节");
        assert_eq!(bytes.exact_bytes, 4);
        assert_eq!(bytes.unresolved, 4, "本地算出来的那 4 字节没有来源");
        assert_eq!(
            bytes.bytes[0].origins[0].address, source,
            "候选来源应是那次读取里对应字节的地址"
        );

        // 近似版本：另外 4 个字节被"连坐"了 —— 这正是误报
        assert!(
            region.candidates > bytes.candidates,
            "近似粒度必须多报（否则这个对比没有意义）"
        );
        assert_eq!(
            region.candidates - bytes.candidates,
            4,
            "多出来的正是被连坐的 4 字节"
        );
        assert!(
            region.machine_line().contains("granularity=region"),
            "机器行要能区分粒度"
        );
    }

    /// 超集关系是**构造性**的：近似粒度的每个候选都必须是字节级候选的超集成员。
    #[test]
    fn the_approximation_is_a_superset_by_construction() {
        let target = 0x7000u64;
        // 读到的 8 字节里，低 4 字节（0x4433_2211）是**唯一**出现的模式；
        // 写入的高 4 字节（0xAABB_CCDD）在读取里根本没有。
        // 于是字节级应当只认 4 个字节，而近似会把整块 8 字节都挂上。
        let events = vec![
            read(1, 0x5100, 8, 0xDEAD_BEEF_4433_2211),
            write(2, target, 8, 0xAABB_CCDD_4433_2211),
        ];
        let bytes = propagate(&events, target, 8, Granularity::Byte);
        let region = propagate(&events, target, 8, Granularity::Region);
        assert_eq!(bytes.candidates, 4, "只有值相等的 4 字节");
        assert_eq!(region.candidates, 8, "近似把整块都挂上");

        for (index, byte) in bytes.bytes.iter().enumerate() {
            let rough = &region.bytes[index];
            for origin in &byte.origins {
                assert!(
                    rough.origins.contains(origin),
                    "近似必须是超集：{:#x} 的候选 {origin:?} 在近似里没有",
                    byte.address
                );
            }
        }
    }

    /// 追溯不到与「没有污点」必须分开：录像前就存在的字节要有自己的计数。
    #[test]
    fn untraceable_bytes_are_counted_separately() {
        let target = 0x8000u64;
        // 没有任何读取，只有一次写入：值无从匹配 ⇒ 全部追溯不到
        let events = vec![write(1, target, 4, 0xAABB_CCDD)];
        let report = propagate(&events, target, 4, Granularity::Byte);
        assert_eq!(report.candidates, 0);
        assert_eq!(report.unresolved, 4);
        assert!(report.render().contains("追溯不到"), "报告要把这件事说出来");
        assert!(
            report.render().contains("**不是**「没有污点」"),
            "要明确它不等于没有污点"
        );
    }

    /// 写入不登记值：否则传播会自证（自己写、自己认）。
    #[test]
    fn a_write_does_not_register_a_readable_value() {
        let target = 0x9000u64;
        let events = vec![
            // 先往 A 写一个值，再把同一个值写到 B —— 没有读取过，就不该有候选
            write(1, 0x5100, 4, 0x1122_3344),
            write(2, target, 4, 0x1122_3344),
        ];
        let report = propagate(&events, target, 4, Granularity::Byte);
        assert_eq!(report.candidates, 0, "没有读取就没有来源");
        assert_eq!(report.unresolved, 4);
    }

    /// 覆盖缺口会把结论降级为下界，报告必须先说这件事。
    #[test]
    fn a_coverage_gap_downgrades_the_answer_to_a_lower_bound() {
        let target = 0x9100u64;
        let events = vec![
            read(1, 0x5200, 4, 0x1122_3344),
            write(2, target, 4, 0x1122_3344),
        ];
        let mut report = propagate(&events, target, 4, Granularity::Byte);
        let without = report.render();
        assert!(!without.contains("下界"), "没有缺口时不该说下界: {without}");
        report.coverage_gap = Some(CoverageGap {
            sites: 10,
            skipped: 2,
            unreadable: 0,
            filtered: 0,
            capped: true,
        });
        let with = report.render();
        assert!(with.contains("下界"), "有缺口时结论只能当集合的下界");
        assert!(with.contains("达到事件上限"), "要说清是什么缺口");
    }

    #[test]
    fn granularity_names_round_trip() {
        assert_eq!(Granularity::parse("byte"), Some(Granularity::Byte));
        assert_eq!(Granularity::parse("region"), Some(Granularity::Region));
        assert_eq!(Granularity::parse("nope"), None);
        assert_eq!(Granularity::default(), Granularity::Byte, "默认用字节级");
    }
}
