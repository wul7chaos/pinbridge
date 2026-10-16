//! 工作台的**数据层**：把真实产物变成界面能直接用的视图。
//!
//! 为什么先做这一层，而不是先做界面：`apps/desktop` 里已经有组件与类型化接口
//! （`HubClient`），但**没有真数据时它证明不了任何事**。界面壳子是最容易写完却
//! 最晚才能验证的东西；而"轨迹读出来是什么、摘要里哪几个数必须常驻可见"才是
//! 这一项真正的内容。
//!
//! 这一层刻意不依赖 Tauri：它只依赖 [`pinbridge_trace`] 与 [`pinbridge_core`]，
//! 因此可以在 CI 里跑用例验证（Tauri 需要 Node + 系统 WebView，已按 README 的
//! 说明**故意排除**在门禁之外）。Tauri 侧将来只是它的一个薄包装：
//!
//! ```text
//! #[tauri::command] fn view_trace(path: String, head: usize) -> Result<TraceView, String>
//! ```
//!
//! 视图里的字段名与 `apps/desktop/src/api/hub.ts` 的接口一一对应；两侧字段名
//! 不同会让联调退化成人工映射，而人工映射必然出错。

use std::collections::BTreeMap;
use std::path::Path;

use pinbridge_core::capability::{Capability, CapabilitySet};
use pinbridge_trace::event::{Event, EventKind};
use pinbridge_trace::pbt::PbtReader;
use pinbridge_trace::slice::{parse_coverage_audit, CoverageGap};
use serde::Serialize;

/// 事件视图（与 `hub.ts` 的 `EventView` 对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventView {
    /// 序号（折叠事件展开后是原序号，`repeats` 说明它代表多少次）。
    pub seq: u64,
    /// 类型短名（见 `pinbridge_trace::event::kind_name`）。
    pub kind: String,
    /// 线程号。
    pub thread: u32,
    /// 事件地址。
    pub address: u64,
    /// 参数（去掉末尾的零）。
    pub args: Vec<u64>,
    /// **折叠出来的重复次数**（`1` 时省略）。
    ///
    /// 注意：这里刻意**不展开**折叠。展开一份百万级事件的轨迹只会把界面和 IPC
    /// 一起压垮，而界面要显示的内容本来就是"这条记录代表 N 次"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeats: Option<u64>,
}

/// 轨迹摘要视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct SummaryView {
    /// 记录条数。
    pub records: u64,
    /// 展开后的事件条数。
    pub events: u64,
    /// 被折叠掉的重复次数。
    pub folded: u64,
    /// 丢弃的事件数。**非 0 必须常驻可见**（见工作台 README）。
    pub dropped: u64,
    /// 是否截断（读到一半就断了）。
    pub truncated: bool,
}

impl SummaryView {
    /// 折叠率（与 `Summary::fold_ratio` 同定义）。
    #[must_use]
    pub fn fold_ratio(&self) -> f64 {
        let total = self.records + self.folded;
        if total == 0 {
            0.0
        } else {
            self.folded as f64 / total as f64
        }
    }
}

/// 一种事件类型及其条数（事件流的状态条要用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindCount {
    /// 类型短名。
    pub kind: String,
    /// **展开后**的条数（折叠也算进去，否则分布会与摘要对不上）。
    pub count: u64,
}

/// 一行的能力矩阵。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityRow {
    /// 能力名（`read_memory` 这样的短名）。
    pub name: String,
    /// 位序号（0 开始）。
    pub bit: u32,
}

/// 轨迹视图：工作台打开一份 `.pbt` 时需要的全部东西。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceView {
    /// 文件路径。
    pub path: String,
    /// 摘要（轨迹里没有汇总段时为 `None` —— Pin 路径上**这是常态**，
    /// 见 `pinbridge_tool.cpp` 的说明：Fini 晚于卸载保护，收尾调用会被拒）。
    pub summary: Option<SummaryView>,
    /// 类型分布（按类型名排序，保证两次读同一文件结果一致）。
    pub kinds: Vec<KindCount>,
    /// 头若干条记录（**折叠保留为 `repeats`**，不展开）。
    pub events: Vec<EventView>,
    /// 记录总数（界面据此显示"还有多少"）。
    pub records: u64,
    /// 是否还有更多记录未取。
    pub has_more: bool,
    /// 采集覆盖（工具写进 `AUDIT` 通道的那一行）。
    pub coverage: Option<CoverageGap>,
    /// 告警。**必须排在数据之前展示**：残缺的结论与完整的结论长得一样，
    /// 不先说清楚就等于骗人。
    pub warnings: Vec<String>,
}

impl TraceView {
    /// 机器可读的一行（与 CLI 其它子命令的 `PB_*` 机器行风格一致）。
    #[must_use]
    pub fn machine_line(&self) -> String {
        let summary_records = self
            .summary
            .as_ref()
            .map_or_else(|| "-".to_owned(), |s| s.records.to_string());
        format!(
            "PB_VIEW path={} records={} summary_records={} kinds={} head={} has_more={} \
             dropped={} capped={} warnings={}",
            self.path,
            self.records,
            summary_records,
            self.kinds.len(),
            self.events.len(),
            u8::from(self.has_more),
            self.summary.as_ref().map_or(0, |s| s.dropped),
            u8::from(self.coverage.is_some_and(|gap| gap.capped)),
            self.warnings.len()
        )
    }

    /// 供 Tauri/HTTP 直接返回的 JSON。
    ///
    /// # Errors
    /// 序列化失败（实际上不会发生：这里全是基本类型），把错误原样交出去而不是
    /// `unwrap`。
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }
}

/// 读一份 `.pbt` 并生成视图。
///
/// `head` 是事件流首屏**记录**条数（折叠不展开，所以这个数就是"行数"）。
///
/// 读取刻意用**流式**模式：Pin 路径上工具收尾会被卸载保护拒绝，文件常常没有
/// `SUMMARY`/`END` 段（这是记录在案的取舍）。用严格模式读这种文件会直接报
/// 损坏——而它恰恰是最常见的那种文件。
///
/// # Errors
/// 文件打不开、或者头部就不是 `.pbt`。
pub fn view_trace(path: &Path, head: usize) -> Result<TraceView, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    let mut reader = PbtReader::open_streaming(file).map_err(|e| e.to_string())?;
    let records = reader.read_all().map_err(|e| e.to_string())?;
    let summary = reader.summary().map_err(|e| e.to_string())?;
    let audit = reader.audit_log().to_owned();
    let finished = reader.end_seen();

    let mut warnings = Vec::new();
    // **判据是 END 段**，不是"有没有 SUMMARY"：写入器刷盘时会写一条 SUMMARY 作为
    // 提交标记，所以没有收尾的轨迹里照样读得到 SUMMARY。Pin 路径上的文件正是如此
    //（工具 Fini 晚于卸载保护，收尾被拒 ⇒ 没有 END）。
    if !finished {
        warnings.push(
            "轨迹**没有收尾段（没有 END）**：按项目已知的取舍，Pin 路径上这是常态\
             （工具收尾晚于目标卸载 DLL，那次调用会被卸载保护拒绝）。已落盘的记录\
             是完整的增量，但**摘要只是刷盘时的提交标记**，不代表录制正常结束。"
                .to_owned(),
        );
    }
    if summary.is_none() && finished {
        warnings.push("轨迹收尾了但没有汇总段：写入方可能没有写 SUMMARY。".to_owned());
    }
    if let Some(summary) = &summary {
        if summary.dropped != 0 {
            warnings.push(format!(
                "轨迹自报丢弃了 {} 条事件：任何「这里没有出现过」的结论都要打折看。",
                summary.dropped
            ));
        }
        if summary.truncated {
            warnings.push("轨迹被截断：结尾部分的事件不完整。".to_owned());
        }
    }

    let coverage = parse_coverage_audit(&audit);
    if let Some(gap) = coverage {
        if gap.capped {
            warnings.push(
                "录制时探针达到事件上限并停止采集：上限之后的事件**不存在**——\
                 那只表示「没采到」，不表示「没发生」。"
                    .to_owned(),
            );
        }
        if gap.skipped != 0 || gap.unreadable != 0 {
            warnings.push(format!(
                "录制时存在采集缺口（跳过写入点 {} 个、读不出 {} 条）：\
                 「没有观察到」不等于「没有发生」。",
                gap.skipped, gap.unreadable
            ));
        }
    }

    // 折叠解成 `repeats`：**不展开**。分布按"记录数 + 折叠次数"算，与摘要一致。
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut rows: Vec<EventView> = Vec::new();
    let mut total_records = 0u64;
    for record in &records {
        total_records += 1;
        if record.kind == EventKind::Repeat {
            // 折叠标记归到它代表的那条记录上；它自己不出现在事件流里。
            if let Some(last) = rows.last_mut() {
                let extra = record.arg(0);
                *last.repeats.get_or_insert(1) += extra;
                if let Some(count) = kinds.get_mut(last.kind.as_str()) {
                    *count += extra;
                }
            }
            continue;
        }
        *kinds.entry(record.kind.name().to_owned()).or_insert(0) += 1;
        if rows.len() < head {
            rows.push(event_view(record));
        }
    }

    Ok(TraceView {
        path: path.display().to_string(),
        summary: summary.map(|summary| SummaryView {
            records: summary.records,
            events: summary.events,
            folded: summary.repeat_folded,
            dropped: summary.dropped,
            truncated: summary.truncated,
        }),
        kinds: kinds
            .into_iter()
            .map(|(kind, count)| KindCount { kind, count })
            .collect(),
        events: rows,
        records: total_records,
        has_more: total_records > head as u64,
        coverage,
        warnings,
    })
}

fn event_view(event: &Event) -> EventView {
    let mut args: Vec<u64> = event.args.to_vec();
    while args.last() == Some(&0) {
        args.pop();
    }
    EventView {
        seq: event.sequence,
        kind: event.kind.name().to_owned(),
        thread: event.thread_id,
        address: event.address,
        args,
        repeats: None,
    }
}

/// 能力矩阵：**唯一真相**是 `pinbridge_core::capability::Capability::ALL`，
/// 界面不自己列一份（列两份就一定会漂移）。
#[must_use]
pub fn capability_matrix() -> Vec<CapabilityRow> {
    Capability::ALL
        .iter()
        .enumerate()
        .map(|(bit, capability)| CapabilityRow {
            name: capability.name().to_owned(),
            bit: bit as u32,
        })
        .collect()
}

/// 能力矩阵的 JSON（工作台底部那些格子的数据源）。
///
/// # Errors
/// 序列化失败（实际上不会发生），把错误原样交出去。
pub fn capability_matrix_json() -> Result<String, String> {
    serde_json::to_string(&capability_matrix()).map_err(|e| e.to_string())
}

/// 把能力集转成"矩阵 + 当前是否已授予"的形态：界面用它显示每一格。
#[must_use]
pub fn capability_state(set: CapabilitySet) -> Vec<(CapabilityRow, bool)> {
    capability_matrix()
        .into_iter()
        .map(|row| {
            let granted = Capability::ALL
                .get(row.bit as usize)
                .is_some_and(|capability| set.contains(*capability));
            (row, granted)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_trace::event::kind_name;
    use pinbridge_trace::pbt::PbtWriter;

    fn with_trace_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pb-workbench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建立临时目录");
        dir
    }

    /// 一份带折叠的轨迹：1 条断点 + 1 条内存写入 + 该写入的 299 次重复。
    fn write_folded(dir: &Path, with_audit: bool) -> std::path::PathBuf {
        let path = dir.join("fold.pbt");
        let file = std::fs::File::create(&path).expect("建立轨迹");
        let mut writer = PbtWriter::create(file, 0).expect("建立写入器");
        writer
            .meta("{\"tool\":\"pinbridge-workbench-test\"}")
            .expect("写 meta");
        if with_audit {
            writer
                .audit_line(
                    "pb_coverage mem_write_sites=1452 skipped_no_after=245 unreadable=0 \
                     filtered=0 capped=1",
                )
                .expect("写审计行");
        }
        let hit = Event::new(1, EventKind::BreakpointHit, 7, 0x1000);
        writer.write_record(&hit).expect("写记录");
        let write = Event::new(2, EventKind::MemWrite, 7, 0x2000)
            .with_arg(0, 0x6000)
            .with_arg(1, 8)
            .with_arg(2, 0xAABB);
        writer.write_record(&write).expect("写记录");
        let repeat = Event::new(3, EventKind::Repeat, 0, 0).with_arg(0, 299);
        writer.write_record(&repeat).expect("写折叠标记");
        writer.finish().expect("收尾");
        path
    }

    /// 折叠**不展开**，而是变成 `repeats`：界面要显示「这条代表 300 次」，
    /// 而不是收到 300 行（百万级事件那样会把界面和 IPC 一起压垮）。
    #[test]
    fn folded_records_become_repeats_instead_of_being_expanded() {
        let dir = with_trace_dir("fold");
        let path = write_folded(&dir, false);
        let view = view_trace(&path, 10).unwrap();
        let summary = view.summary.as_ref().expect("有汇总");
        assert_eq!(summary.records, 3, "记录数：断点 + 写入 + 折叠标记");
        assert_eq!(summary.events, 301, "展开后：1 + 1 + 299 次重复");
        assert_eq!(summary.folded, 299);

        assert_eq!(view.records, 3);
        assert_eq!(view.events.len(), 2, "首屏只有 2 行（折叠不展开）");
        assert_eq!(view.events[0].kind, "breakpoint_hit");
        assert_eq!(view.events[1].kind, "mem_write");
        assert_eq!(
            view.events[1].repeats,
            Some(300),
            "300 = 1 次本体 + 299 次重复"
        );
        assert!(!view.has_more, "3 条记录 < head=10");
        assert!(
            view.kinds
                .iter()
                .any(|kind| kind.kind == "mem_write" && kind.count == 300),
            "分布要按展开后的条数算，否则与摘要对不上: {:?}",
            view.kinds
        );
        assert!(
            !view.kinds.iter().any(|kind| kind.kind == "repeat"),
            "折叠标记不该作为一种事件类型出现: {:?}",
            view.kinds
        );
        assert!(view.machine_line().contains("records=3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 告警必须**排在数据之前**：残缺的结论与完整的结论长得一样。
    #[test]
    fn warnings_are_reported_before_the_data() {
        let dir = with_trace_dir("warn");
        let path = write_folded(&dir, true);
        let view = view_trace(&path, 5).unwrap();
        assert!(
            view.coverage.is_some_and(|gap| gap.capped),
            "AUDIT 里的覆盖要读出来"
        );
        assert!(
            view.warnings.iter().any(|w| w.contains("达到事件上限")),
            "上限缺口必须出现在告警里: {:?}",
            view.warnings
        );
        assert!(
            view.warnings.iter().any(|w| w.contains("跳过写入点 245")),
            "采集缺口也要报: {:?}",
            view.warnings
        );
        let json = view.to_json().unwrap();
        assert!(json.contains("\"warnings\""), "{json}");
        assert!(
            json.contains("\"capped\":true"),
            "覆盖要进 JSON（界面据此常驻显示）: {json}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没有汇总段的轨迹（Pin 路径上的常态）必须能读，并**说清楚为什么没有**。
    #[test]
    fn an_unfinished_trace_is_readable_and_explained() {
        let dir = with_trace_dir("nosummary");
        let path = dir.join("truncated.pbt");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut writer = PbtWriter::create(file, 0).unwrap();
            writer.meta("{\"tool\":\"test\"}").unwrap();
            let event = Event::new(1, EventKind::SyscallEnter, 1, 0x3000).with_arg(0, 0x40);
            writer.write_record(&event).unwrap();
            // **刷盘但故意不调用 finish()**：这才是 Pin 路径上真实文件的形状
            //（增量落盘保证记录在盘上；收尾被卸载保护拒绝，所以没有 SUMMARY/END）。
            // 不刷盘的话记录还在缓冲里，文件里只有 meta —— 那样测的就不是这件事。
            writer.flush().unwrap();
        }
        let view = view_trace(&path, 5).unwrap();
        assert!(
            view.warnings.iter().any(|w| w.contains("没有收尾段")),
            "要用 END 段作为判据并解释原因，而不是当成损坏: {:?}",
            view.warnings
        );
        assert!(
            !view.warnings.iter().any(|w| w.contains("没有汇总段")),
            "刷盘会写提交标记 SUMMARY，所以不能说「没有汇总段」: {:?}",
            view.warnings
        );
        assert_eq!(view.events.len(), 1, "已经落盘的记录仍然要能看见");
        assert_eq!(view.events[0].kind, "syscall_enter");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 能力矩阵来自**单一真相**：12 条、位序号连续、名字可读。
    #[test]
    fn the_capability_matrix_comes_from_the_single_source() {
        let matrix = capability_matrix();
        assert_eq!(matrix.len(), Capability::ALL.len());
        for (index, row) in matrix.iter().enumerate() {
            assert_eq!(row.bit as usize, index, "位序号必须与 ALL 的顺序一致");
            assert!(!row.name.is_empty(), "能力名不能为空");
        }
        let state = capability_state(Capability::read_only_baseline());
        assert_eq!(state.len(), matrix.len());
        let granted: Vec<&str> = state
            .iter()
            .filter(|(_, granted)| *granted)
            .map(|(row, _)| row.name.as_str())
            .collect();
        assert!(!granted.is_empty(), "只读基线至少要开通一项");
        assert!(
            granted.len() < matrix.len(),
            "只读基线必须是**真子集**（否则界面上的能力差异就没意义了）: {granted:?}"
        );
        let full = capability_state(Capability::operator_default());
        assert_eq!(
            full.iter().filter(|(_, granted)| *granted).count(),
            matrix.len(),
            "操作者默认集应当覆盖全部能力"
        );
    }

    /// 类型名表只有一份，且未知判别值不 panic（轨迹可能来自更新的录制方）。
    #[test]
    fn kind_names_come_from_the_shared_table() {
        assert_eq!(kind_name(0), "reserved");
        assert_eq!(kind_name(1), "breakpoint_hit");
        assert_eq!(kind_name(12), "repeat");
        assert_eq!(kind_name(250), "unknown");
        assert_eq!(
            kind_name(EventKind::MemWrite as u32),
            "mem_write",
            "枚举判别值与名字表必须对应"
        );
    }
}
