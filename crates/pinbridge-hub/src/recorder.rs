//! 轨迹录制器：把 Hub 侧的事件流写进 `.pbt`。
//!
//! 记录的是**折叠后**的记录（见 `pinbridge_trace::fold`），
//! 因为落盘前的压缩是唯一不消耗目标进程 CPU 的机会窗口。
//!
//! 一个容易被忽略的细节：`stop()` 必须由调用方显式触发。
//! 进程被杀时文件会缺少 `SUMMARY`/`END` 段——这不是缺陷，而是**证据**：
//! 读侧因此能区分"正常结束的轨迹"与"被中断的轨迹"。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use pinbridge_core::error::{Error, Result};
use pinbridge_trace::event::Event;
use pinbridge_trace::fold::RepeatFolder;
use pinbridge_trace::pbt::{PbtWriter, Summary, FLAG_REPEAT_FOLDED};

/// 录制器。持有文件句柄与折叠状态。
#[derive(Debug)]
pub struct TraceRecorder {
    writer: Option<PbtWriter<BufWriter<File>>>,
    path: PathBuf,
    folder: RepeatFolder,
    folding: bool,
    buffered: Vec<Event>,
    /// 折叠回调里无法向上传播错误，暂存到这里，下次 `record` 时抛出。
    deferred_error: Option<String>,
}

impl TraceRecorder {
    /// 开始录制。
    ///
    /// `meta_json` 会原样写进 META 段：样本哈希、架构、启动参数、工具版本都在这里。
    /// 没有元数据的轨迹在两周后将无法复现，也就无法作为结论的依据。
    pub fn start(path: impl AsRef<Path>, meta_json: &str, folding: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = File::create(&path)?;
        let flags = if folding { FLAG_REPEAT_FOLDED } else { 0 };
        let mut writer = PbtWriter::create(BufWriter::new(file), flags)?;
        writer.meta(meta_json)?;
        Ok(Self {
            writer: Some(writer),
            path,
            folder: RepeatFolder::new(),
            folding,
            buffered: Vec::with_capacity(64),
            deferred_error: None,
        })
    }

    /// 记录一条原始事件（折叠由录制器内部完成）。
    pub fn record(&mut self, event: &Event) -> Result<()> {
        if let Some(message) = self.deferred_error.take() {
            return Err(Error::Trace(message));
        }
        if !self.folding {
            return self.write_one(*event);
        }
        let mut batch = std::mem::take(&mut self.buffered);
        batch.clear();
        let mut failure: Option<String> = None;
        {
            let mut writer = self.writer.as_mut();
            self.folder.push(*event, &mut |folded| {
                if failure.is_some() {
                    return;
                }
                match writer.as_mut() {
                    Some(w) => {
                        if let Err(e) = w.write_record(&folded) {
                            failure = Some(e.to_string());
                        }
                    }
                    None => failure = Some("录制器已关闭".into()),
                }
            });
        }
        self.buffered = batch;
        if let Some(message) = failure {
            self.deferred_error = Some(message.clone());
            return Err(Error::Trace(message));
        }
        Ok(())
    }

    /// 记录一次"背压丢弃"的计数。写进 SUMMARY，让读侧知道轨迹不完整。
    pub fn note_dropped(&mut self, dropped: u64) {
        if let Some(writer) = self.writer.as_mut() {
            writer.note_dropped(dropped);
        }
    }

    /// 轨迹文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 是否启用了折叠。
    #[must_use]
    pub const fn folding(&self) -> bool {
        self.folding
    }

    /// 当前累计汇总。
    #[must_use]
    pub fn summary(&self) -> Option<&Summary> {
        self.writer.as_ref().map(PbtWriter::summary)
    }

    /// 停止录制并写尾。返回最终汇总。
    pub fn stop(mut self) -> Result<Summary> {
        let Some(mut writer) = self.writer.take() else {
            return Err(Error::Trace("录制器已经停止".into()));
        };
        // 冲刷最后一个折叠段：漏掉它会让轨迹尾部凭空少掉一大段
        let mut failure: Option<String> = None;
        {
            let mut records: Vec<Event> = Vec::new();
            self.folder.finish(&mut |folded| records.push(folded));
            for record in records {
                if let Err(e) = writer.write_record(&record) {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(message) = failure {
            return Err(Error::Trace(message));
        }
        let (inner, summary) = writer.finish()?;
        let mut file = inner
            .into_inner()
            .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
        file.flush()?;
        Ok(summary)
    }
}

impl TraceRecorder {
    /// 不折叠地直接写一条记录。
    fn write_one(&mut self, event: Event) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(Error::Trace("录制器已停止".into()));
        };
        writer.write_record(&event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinbridge_core::id::EventSeq;
    use pinbridge_trace::event::EventKind;
    use pinbridge_trace::fold::expand_records;
    use pinbridge_trace::pbt::PbtReader;

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "pinbridge-hub-test-{}-{}",
            std::process::id(),
            name
        ));
        dir.push("trace.pbt");
        dir
    }

    fn fixture_events() -> Vec<Event> {
        let mut events = vec![Event::new(1, EventKind::BreakpointHit, 7, 0x1000).with_arg(0, 0xAB)];
        for i in 0..2000u64 {
            events.push(
                Event::new(2 + i, EventKind::SyscallEnter, 7, 0x2000)
                    .with_arg(0, 0x40)
                    .with_arg(1, 0x3000),
            );
        }
        events
    }

    #[test]
    fn folding_recorder_roundtrips_through_disk() {
        let path = temp_path("fold");
        let events = fixture_events();
        let mut recorder = TraceRecorder::start(&path, r#"{"sample":"fixture"}"#, true).unwrap();
        for event in &events {
            recorder.record(event).unwrap();
        }
        recorder.note_dropped(3);
        let summary = recorder.stop().unwrap();
        assert_eq!(summary.events, events.len() as u64);
        assert_eq!(summary.repeat_folded, 1999);
        assert_eq!(summary.dropped, 3);
        assert!(summary.fold_ratio() > 0.99);

        let mut reader = PbtReader::open(File::open(&path).unwrap()).unwrap();
        let records = reader.read_all().unwrap();
        assert_eq!(expand_records(&records), events, "落盘 + 读回必须无损");
        assert!(reader.meta().contains("fixture"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unfiltered_recorder_writes_every_event() {
        let path = temp_path("raw");
        let events = fixture_events();
        let mut recorder = TraceRecorder::start(&path, "{}", false).unwrap();
        for event in &events {
            recorder.record(event).unwrap();
        }
        let summary = recorder.stop().unwrap();
        assert_eq!(summary.records, events.len() as u64);
        assert_eq!(summary.repeat_folded, 0);
        let mut reader = PbtReader::open(File::open(&path).unwrap()).unwrap();
        assert_eq!(reader.read_all().unwrap(), events);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn recorder_tracks_latest_summary_before_stop() {
        let path = temp_path("progress");
        let mut recorder = TraceRecorder::start(&path, "{}", true).unwrap();
        recorder
            .record(&Event::new(1, EventKind::StateChange, 1, 0))
            .unwrap();
        let seq: EventSeq = EventSeq::new(1);
        assert_eq!(seq.get(), 1);
        assert!(recorder.summary().is_some_and(|s| s.records >= 1));
        assert!(recorder.folding());
        assert!(recorder.path().ends_with("trace.pbt"));
        let _ = recorder.stop().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
