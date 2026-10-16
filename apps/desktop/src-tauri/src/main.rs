//! PinBridge 工作台的 Tauri 壳。
//!
//! 这一层**刻意做薄**：命令只负责把界面要的数据转成 JSON，逻辑全在
//! `pinbridge-workbench`（那里有用例，且不依赖 Tauri ⇒ 能进 CI）。
//!
//! 为什么这样切分：界面壳子最容易写完却最晚能验证。如果把"轨迹读出来是什么、
//! 哪几个数必须常驻可见"写进这里，它们就只能靠"手动点一遍"来验证；
//! 放在 `pinbridge-workbench` 里则可以用 `cargo test` 钉住。
//!
//! 命令与 `apps/desktop/src/state/useViewPayload.ts` 的调用一一对应：
//!
//! ```text
//! invoke("view_trace", { path, head })   → TraceView 的 JSON
//! invoke("capability_matrix")            → 能力矩阵的 JSON
//! invoke("coverage_audit", { path })     → 只取覆盖缺口（诊断用）
//! ```

/// 读一份轨迹并返回视图 JSON。
///
/// 错误用 `Err(String)` 交回前端：界面会把失败原因**显示在告警区**
/// （而不是白屏或假装成功）—— 见 `useViewPayload.ts` 的说明。
#[tauri::command]
fn view_trace(path: String, head: usize) -> Result<String, String> {
    let view = pinbridge_workbench::view_trace(std::path::Path::new(&path), head)?;
    view.to_json()
}

/// 能力矩阵（唯一真相在 `pinbridge-core`）。
#[tauri::command]
fn capability_matrix() -> Result<String, String> {
    pinbridge_workbench::capability_matrix_json()
}

/// 只取覆盖缺口：面板上"这份数据缺了多少"的那一格。
#[tauri::command]
fn coverage_audit(path: String) -> Result<String, String> {
    let view = pinbridge_workbench::view_trace(std::path::Path::new(&path), 0)?;
    serde_json::to_string(&view.coverage).map_err(|e| e.to_string())
}

/// 组装应用。单独抽出来是为了让 `main` 保持一行，也便于将来在测试里
/// 构造一个不带窗口的实例。
pub fn build_app() -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            view_trace,
            capability_matrix,
            coverage_audit
        ])
}

fn main() {
    build_app()
        .run(tauri::generate_context!())
        .expect("启动 PinBridge 工作台");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_trace::event::{Event, EventKind};
    use pinbridge_trace::pbt::PbtWriter;

    /// 造一份真实形状的轨迹（含覆盖记录 + 折叠）。
    fn write_trace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pb-desktop-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).expect("建立目录");
        let path = dir.join("trace.pbt");
        let file = std::fs::File::create(&path).expect("建立轨迹");
        let mut writer = PbtWriter::create(file, 0).expect("建立写入器");
        writer.meta("{\"tool\":\"desktop-test\"}").expect("写 meta");
        writer
            .audit_line(
                "pb_coverage mem_write_sites=1452 skipped_no_after=245 unreadable=0 filtered=0 capped=1",
            )
            .expect("写审计行");
        let hit = Event::new(1, EventKind::BreakpointHit, 7, 0x1000);
        writer.write_record(&hit).expect("写记录");
        let write = Event::new(2, EventKind::MemWrite, 7, 0x2000).with_arg(0, 0x6000);
        writer.write_record(&write).expect("写记录");
        let repeat = Event::new(3, EventKind::Repeat, 0, 0).with_arg(0, 299);
        writer.write_record(&repeat).expect("写折叠标记");
        writer.flush().expect("刷盘（模拟收尾被拒的真实文件）");
        path
    }

    /// 命令层必须真的把**真实轨迹**变成界面能用的 JSON。
    ///
    /// 这条用例直接调命令函数（不经过 Tauri 运行时）：命令体里唯一可能出错的东西
    /// 就是"转发姿势"——路径、参数、JSON 形状。窗口与消息循环不是这里要验证的对象。
    #[test]
    fn the_view_trace_command_returns_a_payload_the_ui_can_use() {
        let path = write_trace("view");
        let json = view_trace(path.display().to_string(), 5).expect("命令应当成功");
        let payload: serde_json::Value = serde_json::from_str(&json).expect("必须是合法 JSON");

        assert!(payload["path"].is_string());
        assert_eq!(payload["records"], 3, "3 条记录（含折叠标记）");
        assert_eq!(payload["events"].as_array().map(Vec::len), Some(2), "折叠不展开");
        assert_eq!(payload["events"][1]["repeats"], 300);
        assert_eq!(payload["coverage"]["capped"], true);
        let warnings = payload["warnings"].as_array().expect("要有告警数组");
        assert!(
            warnings.iter().any(|w| w.as_str().is_some_and(|t| t.contains("没有收尾段"))),
            "未收尾的轨迹必须被说明: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.as_str().is_some_and(|t| t.contains("上限"))),
            "覆盖缺口要进告警: {warnings:?}"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    /// 读不存在的文件要**报错**，不是返回空载荷：界面据此显示失败原因。
    #[test]
    fn the_view_trace_command_reports_failure_instead_of_faking_an_empty_view() {
        let error = view_trace("Z:\\definitely-not-here\\nope.pbt".to_owned(), 5)
            .expect_err("不存在的文件必须报错");
        assert!(error.contains("打开"), "错误信息要说清是什么失败: {error}");
    }

    /// 能力矩阵走的是**同一份真相**（12 项，位序号连续）。
    #[test]
    fn the_capability_matrix_command_reports_the_single_source() {
        let json = capability_matrix().expect("命令应当成功");
        let rows: Vec<serde_json::Value> = serde_json::from_str(&json).expect("必须是数组");
        assert_eq!(rows.len(), 12, "能力矩阵与 pinbridge-core 一致");
        assert_eq!(rows[0]["bit"], 0);
        assert!(
            rows.iter().any(|row| row["name"] == "mem_read"),
            "能力名要与核心一致: {rows:?}"
        );
    }

    /// 覆盖缺口单独可查（面板上那一格）。
    #[test]
    fn the_coverage_audit_command_exposes_the_gap_alone() {
        let path = write_trace("coverage");
        let json = coverage_audit(path.display().to_string()).expect("命令应当成功");
        let gap: serde_json::Value = serde_json::from_str(&json).expect("必须是 JSON");
        assert_eq!(gap["skipped"], 245);
        assert_eq!(gap["capped"], true);
        let _ = std::fs::remove_dir_all(path.parent().expect("有父目录"));
    }
}
