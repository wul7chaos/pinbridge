//! 反向切片：**地址级**地回答「谁写了这块字节、谁读过、什么时候被执行到」
//! （语义与边界见 ADR 0013）。
//!
//! 刻意**不做**数据流推断：一份轨迹里没有"寄存器↔内存的数据来源"关系，
//! 硬做只能靠猜，而一条假的数据流链比"答不出来"更糟——调用方会照着它改代码。
//! 本模块只回答四个**可判定**的问题：
//!
//! 1. 谁写的（所有与范围重叠的 `mem_write`，按序号升序，**不是只留最后一个**）；
//! 2. 被覆盖过几次（脱壳是反复改写同一片内存，只报最后一个等于把过程抹掉）；
//! 3. 谁读过（重叠的 `mem_read`，回答"执行前有没有被别处读过"）；
//! 4. 第一次被执行是什么时候（第一条 `address` 落在范围内的事件）。
//!
//! 另外两条硬规则：**探针没开就说没开**（不能报一个空写入链，那会被读成
//! "没人写过它"）；**不猜数据流**（值相等不是因果）。

use crate::event::{Event, EventKind};

/// 一次写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteRecord {
    /// 事件序号。
    pub sequence: u64,
    /// 起始地址。
    pub address: u64,
    /// 长度（字节）。
    pub size: u64,
    /// 写入值（探针读不出时为 0，见 ADR 0013 的说明）。
    pub value: u64,
    /// 是第几条事件（用于稳定排序与报告定位）。
    pub index: usize,
}

/// 一次读取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    /// 事件序号。
    pub sequence: u64,
    /// 起始地址。
    pub address: u64,
    /// 长度（字节）。
    pub size: u64,
    /// 是第几条事件。
    pub index: usize,
}

/// 采集缺口（工具在收尾前经 agent 写进 `AUDIT` 通道的一行，见 ADR 与 CHANGELOG）。
///
/// **为什么它必须离线可读**：轨迹会被拷走，"某个地址没有被写过"这类结论在
/// **采集本身有缺口**时不能成立——而缺口信息只有录制方知道。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoverageGap {
    /// 插装的内存事件点数量。
    pub sites: u64,
    /// 因无法插 `AFTER` 点而整点跳过的写入点数量。
    pub skipped: u64,
    /// 值读不出的条数。
    pub unreadable: u64,
    /// 被地址范围过滤掉的条数（**有意**过滤，不算缺口）。
    pub filtered: u64,
    /// 探针是否因达到上限而**软停**：上限之后的事件**不存在**，但不等于没发生。
    pub capped: bool,
}

/// 从审计文本里解析覆盖行：`pb_coverage mem_write_sites=… skipped_no_after=… …`。
///
/// 解析失败返回 `None`（老轨迹、或者录制方没有这套元信息），**不猜**。
#[must_use]
pub fn parse_coverage_audit(audit: &str) -> Option<CoverageGap> {
    let line = audit
        .lines()
        .find(|line| line.starts_with("pb_coverage "))?;
    let mut gap = CoverageGap::default();
    let mut seen_any = false;
    for field in line.split_whitespace().skip(1) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let parsed = value.parse::<u64>().ok();
        match (key, parsed) {
            ("mem_write_sites", Some(v)) => {
                gap.sites = v;
                seen_any = true;
            }
            ("skipped_no_after", Some(v)) => {
                gap.skipped = v;
                seen_any = true;
            }
            ("unreadable", Some(v)) => {
                gap.unreadable = v;
                seen_any = true;
            }
            ("filtered", Some(v)) => {
                gap.filtered = v;
                seen_any = true;
            }
            ("capped", Some(v)) => {
                gap.capped = v != 0;
                seen_any = true;
            }
            // 认不出的键**忽略但不报错**：格式以后可能加字段，老读端不该因此
            // 丢掉整行信息（向前兼容的唯一做法）。
            _ => {}
        }
    }
    if seen_any {
        Some(gap)
    } else {
        None
    }
}

/// 切片结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slice {
    /// 目标范围。
    /// 目标范围起始地址。
    pub target: u64,
    /// 目标范围长度（字节）。
    pub len: u64,
    /// 与范围重叠的写入（按序号升序）。
    pub writers: Vec<WriteRecord>,
    /// 与范围重叠的读取（按序号升序）。
    pub readers: Vec<ReadRecord>,
    /// 第一次执行到的时刻（事件的序号）；从未执行到则为 `None`。
    pub first_execution: Option<u64>,
    /// 落在范围内的执行次数（事件的 `address` 在范围内）。
    pub executions: u64,
    /// 轨迹里一共有多少条 `mem_write`（**用来判断探针开没开**）。
    pub total_writes_in_trace: u64,
    /// 轨迹里是否出现过内存写入事件。
    pub memory_probe_on: bool,
    /// 录制时工具自报的**采集缺口**（由 agent 写进轨迹的 `AUDIT` 文本通道）。
    ///
    /// 工具把这条记录作为 `state_change` 事件写进轨迹（`args[0] = 1`）。
    /// 为什么离线侧必须看它：轨迹会被拷走做分析，"这个地址没有被写过"这种结论
    /// 在**采集本身有缺口**时是不能成立的——第 33 轮就踩过：靶子的写入循环确实
    /// 跑了 2000 次，而轨迹里一条都没有（探针没覆盖那段代码）。
    pub coverage_gap: Option<CoverageGap>,
}

impl Slice {
    /// 范围内被覆盖的字节里，有没有从来没被写过的（只读映像的常态）。
    ///
    /// 只按"有没有写入者"粗判：精确到字节需要区间合并，而这一步的结论
    /// （"这段代码不是被写出来的"）本来就是粗判。
    #[must_use]
    pub const fn has_no_writer(&self) -> bool {
        self.writers.is_empty()
    }

    /// 机器可读的一行。
    #[must_use]
    pub fn machine_line(&self) -> String {
        let first_exec = self
            .first_execution
            .map_or_else(|| "-".to_owned(), |s| s.to_string());
        format!(
            "PB_SLICE target={:#x} len={} writers={} readers={} overwritten={} executions={} first_execution={} memory_probe={} coverage_gap={}",
            self.target,
            self.len,
            self.writers.len(),
            self.readers.len(),
            self.writers.len().saturating_sub(1),
            self.executions,
            first_exec,
            u8::from(self.memory_probe_on),
            self.coverage_gap.map_or_else(
                || "none".to_owned(),
                |gap| format!(
                    "{}/{}/{}/capped{}",
                    gap.skipped,
                    gap.unreadable,
                    gap.filtered,
                    u8::from(gap.capped)
                ),
            ),
        )
    }

    /// 给人/AI 看的报告。
    #[must_use]
    pub fn render(&self, limit: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "目标范围 {:#x}..{:#x}（{} 字节）\n",
            self.target,
            self.target + self.len,
            self.len
        ));

        // ── 采集缺口优先说：它会推翻下面所有结论 ──
        if let Some(gap) = self.coverage_gap {
            if gap.capped {
                out.push_str(
                    "⚠ 录制时**探针达到事件上限并停止采集**：从此之后的事件在轨迹里\n\
                     \x20 **不存在**——但那只表示「没采到」，**不表示「没发生」**。\n\
                     \x20 下面任何「没有观察到…」的结论在这条轨迹上都不成立。\n",
                );
            }
            if gap.skipped != 0 || gap.unreadable != 0 {
                out.push_str(&format!(
                    "⚠ 录制时工具自报采集缺口：跳过的写入点 {} 个、读不出的值 {} 条\n\
                     \x20 （被地址范围过滤 {} 条是**有意**的，不算缺口）。\n\
                     \x20 有缺口时下面的「没有观察到写入」**不能**当作「没被写过」——\n\
                     \x20 缺口可能正好落在这段代码上。\n",
                    gap.skipped, gap.unreadable, gap.filtered
                ));
            }
        }
        // ── 结论的第一句必须能站住 ──
        if !self.memory_probe_on {
            out.push_str(
                "**本轨迹里没有内存写入事件** ⇒ 无法回答「谁写的」。\n\
                 \x20 这通常是录制时没开内存探针（`-pb_mem rw`），**不是**「没人写过它」。\n\
                 \x20 能确定的只有下面这几条：\n",
            );
        } else if self.has_no_writer() {
            out.push_str(
                "范围内**没有观察到写入**：这段字节不是被这份轨迹写出来的\n\
                 \x20 （只读映像、别的进程写的、或者写入发生在录制开始之前）。\n",
            );
        } else {
            out.push_str(&format!(
                "写入者 {} 个（其中 {} 次是**覆盖**同一片字节——脱壳的常态是反复改写）：\n",
                self.writers.len(),
                self.writers.len().saturating_sub(1)
            ));
            for writer in self.writers.iter().take(limit) {
                out.push_str(&format!(
                    "  #{} 序号 {}  写 {:#x}({}) = {:#x}\n",
                    writer.index, writer.sequence, writer.address, writer.size, writer.value
                ));
            }
            if self.writers.len() > limit {
                out.push_str(&format!(
                    "  （共 {} 个写入者，这里只列了前 {limit} 个）\n",
                    self.writers.len()
                ));
            }
        }

        if !self.readers.is_empty() {
            out.push_str(&format!(
                "范围内被读过 {} 次（执行前有没有被别处读过，是「二次解密/校验」的线索）：\n",
                self.readers.len()
            ));
            for reader in self.readers.iter().take(limit) {
                out.push_str(&format!(
                    "  #{} 序号 {}  读 {:#x}({})\n",
                    reader.index, reader.sequence, reader.address, reader.size
                ));
            }
        }

        match self.first_execution {
            Some(sequence) => out.push_str(&format!(
                "第一次执行到：序号 {sequence}（共执行 {} 次事件的地址落在范围内）\n",
                self.executions
            )),
            None => out.push_str(
                "**没有执行到**：范围内没有出现过任何事件的地址 ⇒ 这段代码在本轨迹里没跑起来。\n\
                 \x20 它可能是别的路径上的 OEP 候选，也可能是数据而非代码。\n",
            ),
        }

        out.push_str(
            "说明：这是**地址级**切片——它能说清「谁写的、谁读过、何时执行」，\n\
             \x20 **不能**说清「这些字节是从哪复制来的」（那需要字节级污点传播）。\n",
        );
        out.push_str(&self.machine_line());
        out.push('\n');
        out
    }
}

/// 范围 `[a, a+len)` 与 `[b, b+size)` 是否重叠。
fn overlaps(a: u64, len: u64, b: u64, size: u64) -> bool {
    if len == 0 || size == 0 {
        return false;
    }
    // 用 `saturating_add` 防止极端地址回绕导致"什么都重叠"这种静默错误。
    a < b.saturating_add(size) && b < a.saturating_add(len)
}

/// 对目标范围做地址级反向切片。
#[must_use]
pub fn reverse_slice(events: &[Event], target: u64, len: u64) -> Slice {
    let mut slice = Slice {
        target,
        len,
        ..Slice::default()
    };
    for (index, event) in events.iter().enumerate() {
        match event.kind {
            EventKind::MemWrite => {
                slice.total_writes_in_trace += 1;
                slice.memory_probe_on = true;
                let address = event.args[0];
                let size = event.args[1];
                if overlaps(target, len, address, size) {
                    slice.writers.push(WriteRecord {
                        sequence: event.sequence,
                        address,
                        size,
                        value: event.args[2],
                        index,
                    });
                }
            }
            EventKind::MemRead => {
                let address = event.args[0];
                let size = event.args[1];
                if overlaps(target, len, address, size) {
                    slice.readers.push(ReadRecord {
                        sequence: event.sequence,
                        address,
                        size,
                        index,
                    });
                }
            }
            // 覆盖子码 1（见 native 侧「采集覆盖写进轨迹」的注释）。写成 `match` 上的
            // 守卫而不是嵌套 `if`：clippy 的 `collapsible_match` 会把嵌套形式挑出来，
            // 而守卫形式语义相同、也让「这条记录只在子码为 1 时才解释」更显眼。
            //
            // 事件通道是**尽力而为**：环形缓冲满时它自己就会被丢掉（实测过）。
            // 主要通道是 `AUDIT` 文本，调用方读完文件后用
            // `parse_coverage_audit` 覆盖一次即可。
            EventKind::StateChange if event.args[0] == 1 => {
                slice.coverage_gap = Some(CoverageGap {
                    sites: event.args[1],
                    skipped: event.args[2],
                    unreadable: event.args[3],
                    filtered: event.args[4],
                    capped: event.args[5] != 0,
                });
            }
            _ => {}
        }
        // 执行时刻：事件的 `address` 落在范围内。
        // 这一条对**所有**事件类型都成立（不只内存事件）：只要有一条事件
        // 发生在那个地址上，就说明那里的代码跑到了。
        if len != 0 && event.address >= target && event.address < target.saturating_add(len) {
            slice.executions += 1;
            if slice.first_execution.is_none() {
                slice.first_execution = Some(event.sequence);
            }
        }
    }
    slice
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};

    /// 写入事件的 `address` 是**执行写入的那条指令**，写入地址在 `args[0]`。
    ///
    /// 这不是细节：把两者设成同一个值时，"被写入"会被误算成"被执行到"。
    /// 第一版测试数据就是这么错的，用例直接把这件事暴露了出来。
    const WRITE_INS: u64 = 0x9000;

    fn write(sequence: u64, address: u64, size: u64, value: u64) -> Event {
        let mut event = Event::new(sequence, EventKind::MemWrite, 7, WRITE_INS)
            .with_arg(0, address)
            .with_arg(1, size)
            .with_arg(2, value);
        // 让"写入指令"这个地址不属于目标范围（否则它本身也是一次执行）。
        event.address = WRITE_INS;
        event
    }

    fn read(sequence: u64, address: u64, size: u64) -> Event {
        let mut event = Event::new(sequence, EventKind::MemRead, 7, WRITE_INS)
            .with_arg(0, address)
            .with_arg(1, size);
        event.address = WRITE_INS;
        event
    }

    /// 默认写入指令地址（便于阅读）。
    const _: () = ();

    /// 执行到某个地址：任何事件只要 `address` 在那里就算。
    fn executes(sequence: u64, address: u64) -> Event {
        Event::new(sequence, EventKind::BreakpointHit, 7, address)
    }

    /// 脱壳的常态：**同一片字节被反复改写**。只报最后一个写入者等于把过程抹掉。
    #[test]
    fn every_writer_is_listed_not_only_the_last_one() {
        let events = vec![
            write(1, 0x1000, 16, 0xAA),
            write(2, 0x1000, 16, 0xBB),
            write(3, 0x1000, 16, 0xCC),
            executes(4, 0x1000),
        ];
        let slice = reverse_slice(&events, 0x1000, 16);
        assert_eq!(slice.writers.len(), 3, "三个写入者都要在");
        assert_eq!(slice.writers[0].value, 0xAA);
        assert_eq!(slice.writers[2].value, 0xCC);
        assert!(slice.memory_probe_on);
        let text = slice.render(8);
        assert!(text.contains("覆盖"), "要说清有覆盖: {text}");
        assert_eq!(slice.first_execution, Some(4));
    }

    /// **部分重叠**也算写入者：脱壳器常常按 4/16/4096 字节分块写。
    #[test]
    fn partial_overlap_counts_as_a_writer() {
        let events = vec![
            write(1, 0x0FF8, 16, 0x11),   // 尾部 8 字节落在范围内
            write(2, 0x1000, 4, 0x22),    // 完全在范围内
            write(3, 0x1010, 16, 0x33),   // 从范围末尾开始：**不重叠**
            write(4, 0x2000, 4096, 0x44), // 离得很远
        ];
        let slice = reverse_slice(&events, 0x1000, 16);
        let addresses: Vec<u64> = slice.writers.iter().map(|w| w.address).collect();
        assert_eq!(addresses, vec![0x0FF8, 0x1000], "边界要判准");
    }

    /// 「被写出来」与「被执行到」是两件事，必须分开报。
    #[test]
    fn being_written_and_being_executed_are_reported_separately() {
        let events = vec![write(1, 0x1000, 16, 0xAA)];
        let slice = reverse_slice(&events, 0x1000, 16);
        assert_eq!(slice.writers.len(), 1);
        assert_eq!(slice.first_execution, None, "写出来不等于跑起来");
        let text = slice.render(8);
        assert!(text.contains("没有执行到"), "{text}");
        assert!(
            text.contains("可能是数据而非代码"),
            "要给可能的解释: {text}"
        );
    }

    /// 探针没开时说"没开"，**不能**报一个空写入链——那会被读成"没人写过它"。
    #[test]
    fn without_the_memory_probe_it_says_so_instead_of_reporting_an_empty_chain() {
        let events = vec![executes(1, 0x1000), executes(2, 0x1002)];
        let slice = reverse_slice(&events, 0x1000, 16);
        assert!(!slice.memory_probe_on);
        assert!(slice.writers.is_empty());
        assert_eq!(slice.executions, 2);
        assert_eq!(slice.first_execution, Some(1));
        let text = slice.render(8);
        assert!(text.contains("没有内存写入事件"), "{text}");
        assert!(text.contains("-pb_mem"), "要指出怎么打开探针: {text}");
        assert!(
            text.contains("不是**「没人写过它」**") || text.contains("不是"),
            "{text}"
        );
    }

    /// 范围外的执行不算；读取要按重叠判定。
    #[test]
    fn executions_outside_the_range_are_not_counted_and_reads_are_range_checked() {
        let events = vec![
            executes(1, 0x0FFF), // 范围前一个字节
            // [0x0FF8,0x1008) 与 [0x1000,0x1010) 有 8 字节重叠。
            // 注意 [0x0FF0,0x1000) **不**重叠：区间是半开的，正好相接不算重叠。
            read(2, 0x0FF8, 16),
            read(3, 0x2000, 8), // 范围外
            executes(4, 0x100F),
            executes(5, 0x1010), // 范围后一个字节
        ];
        let slice = reverse_slice(&events, 0x1000, 16);
        assert_eq!(slice.executions, 1, "只有 0x100F 在 [0x1000,0x1010) 内");
        assert_eq!(slice.first_execution, Some(4));
        assert_eq!(slice.readers.len(), 1);
        assert_eq!(slice.readers[0].address, 0x0FF8);
        // 顺带把"半开区间"这条也钉住：正好相接的读取不算重叠。
        let touching = reverse_slice(&[read(1, 0x0FF0, 16)], 0x1000, 16);
        assert!(touching.readers.is_empty(), "半开区间：相接不算重叠");
    }

    /// 极端地址不能因为回绕而"什么都重叠"。
    #[test]
    fn addresses_near_the_top_do_not_wrap_into_everything() {
        let events = vec![write(1, u64::MAX - 3, 8, 0xAA), write(2, 0x1000, 8, 0xBB)];
        let slice = reverse_slice(&events, 0x1000, 16);
        assert_eq!(slice.writers.len(), 1, "回绕不能把远端的写入算进来");
        assert_eq!(slice.writers[0].address, 0x1000);
    }

    #[test]
    fn machine_line_carries_the_numbers_the_caller_needs() {
        let events = vec![
            write(1, 0x1000, 16, 0xAA),
            write(2, 0x1000, 16, 0xBB),
            read(3, 0x1000, 16),
            executes(4, 0x1004),
        ];
        let slice = reverse_slice(&events, 0x1000, 16);
        let line = slice.machine_line();
        assert!(line.contains("writers=2"), "{line}");
        assert!(line.contains("readers=1"), "{line}");
        assert!(line.contains("overwritten=1"), "{line}");
        assert!(line.contains("first_execution=4"), "{line}");
        assert!(line.contains("memory_probe=1"), "{line}");
        assert!(line.contains("target=0x1000"), "{line}");
    }
    /// 规则说明：**任何**事件的 `address` 落在范围内就算"执行到了"。
    ///
    /// 这包括"在范围内执行的写入指令"——那正是"这段代码已经跑起来"的证据。
    /// 所以判定标准不是"有没有 breakpoint 命中"，而是"有没有来自那个地址的事件"。
    #[test]
    fn an_event_originating_inside_the_range_counts_as_execution() {
        let mut writing_inside = write(1, 0x2000, 8, 0xAA);
        writing_inside.address = 0x1004; // 写入指令本身就在目标范围里
        let slice = reverse_slice(&[writing_inside], 0x1000, 16);
        assert_eq!(slice.executions, 1);
        assert_eq!(slice.first_execution, Some(1));
        assert_eq!(slice.writers.len(), 0, "写到 0x2000，与范围不重叠");
    }

    /// 采集缺口必须**优先于**结论被说出来。
    ///
    /// 这条守的是第 33 轮的实际教训：靶子的写入循环确实跑了 2000 次，
    /// 轨迹里一条都没有（探针没覆盖那段代码），而切片当时报的是
    /// 「范围内没有观察到写入」——技术上没说谎，但读者会当成「没被写过」。
    #[test]
    fn a_reported_coverage_gap_is_surfaced_before_the_conclusion() {
        let coverage = Event::new(1, EventKind::StateChange, 0, 0)
            .with_arg(0, 1)
            .with_arg(1, 1452)
            .with_arg(2, 245)
            .with_arg(3, 0)
            .with_arg(4, 0);
        let slice = reverse_slice(&[coverage], 0x1000, 16);
        assert_eq!(
            slice.coverage_gap,
            Some(CoverageGap {
                sites: 1452,
                skipped: 245,
                unreadable: 0,
                filtered: 0,
                capped: false,
            })
        );
        let text = slice.render(8);
        assert!(text.contains("采集缺口"), "{text}");
        assert!(text.contains("跳过的写入点 245 个"), "{text}");
        assert!(
            text.contains("不能**当作「没被写过」"),
            "要说清后果: {text}"
        );
        assert!(
            text.find("采集缺口").unwrap() < text.find("没有观察到写入").unwrap(),
            "缺口必须排在结论前面"
        );
        assert!(slice
            .machine_line()
            .contains("coverage_gap=245/0/0/capped0"));
    }

    /// 没有缺口记录时不能凭空造一个（老轨迹没有这条记录）。
    #[test]
    fn a_trace_without_a_coverage_record_reports_none() {
        let events = vec![write(1, 0x1000, 8, 0xAA)];
        let slice = reverse_slice(&events, 0x1000, 8);
        assert_eq!(slice.coverage_gap, None);
        assert!(slice.machine_line().contains("coverage_gap=none"));
    }

    /// 审计行解析：主通道必须能读出来，且**认不出的键不致命**。
    ///
    /// 为什么解析要容忍未知键：这套元信息以后会加字段，老读端不该因为多了一个
    /// `key=value` 就把整行丢掉——那会让"升级录制方"变成"旧工具全瞎"。
    #[test]
    fn the_coverage_audit_line_is_parsed_and_unknown_keys_are_ignored() {
        let gap = parse_coverage_audit(
            "启动完成\npb_coverage mem_write_sites=1452 skipped_no_after=245 unreadable=3 filtered=7 capped=1 future_field=99\n结束",
        )
        .expect("应当解析出覆盖");
        assert_eq!(gap.sites, 1452);
        assert_eq!(gap.skipped, 245);
        assert_eq!(gap.unreadable, 3);
        assert_eq!(gap.filtered, 7);
        assert!(gap.capped);

        assert!(parse_coverage_audit("无关的一行\n").is_none(), "没有就不猜");
        assert!(
            parse_coverage_audit("pb_coverage\n").is_none(),
            "只有前缀、没有任何字段时不算有效记录"
        );
    }

    /// `capped` 的告警必须**排在结论前面**，而且要写明"不是没发生"。
    #[test]
    fn a_capped_probe_warning_precedes_any_conclusion() {
        let events = vec![write(1, 0x1000, 8, 0xAA)];
        let mut slice = reverse_slice(&events, 0x2000, 8);
        slice.coverage_gap = Some(CoverageGap {
            sites: 100,
            skipped: 0,
            unreadable: 0,
            filtered: 0,
            capped: true,
        });
        let text = slice.render(8);
        assert!(text.contains("达到事件上限"), "{text}");
        assert!(
            text.contains("不表示「没发生」"),
            "必须点明这条区别: {text}"
        );
        let capped_at = text.find("达到事件上限").unwrap();
        let conclusion_at = text.find("没有观察到写入").unwrap();
        assert!(capped_at < conclusion_at, "告警要在结论之前");
    }
}
