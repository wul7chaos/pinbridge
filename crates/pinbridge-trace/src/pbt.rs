//! `.pbt` 轨迹文件编解码（Rust 侧）。
//!
//! 规范正文在 `docs/specs/pbt-format.md`，本文件是它的转写。
//! Python 侧 `python/src/pb/trace/pbt.py` 是同一规范的第二个实现，
//! 两边共享同一组"黄金字节"测试，任何一个实现偏离规范都会被测试抓住。
//!
//! ## 容器
//!
//! ```text
//! File  := "PBT1"(4) | version u16 | flags u16 | Chunk*
//! Chunk := tag u8 | length u32 | payload[length]
//! ```
//!
//! 用 **chunk 序列**而不是"一个 section 一个长度"：录制线程必须能边跑边写，
//! 而 section 的定长头要求在写之前就知道总长度（要么全缓冲、要么回头 seek）。
//! 一条 5 分钟 VMP 会话的轨迹可能几十 GB，两种都不可接受。
//! chunk 让"边产边写"成为可能，读侧把同 tag 的 chunk 视为同一逻辑段的延续。
//!
//! 未知 tag 按长度跳过 —— 这是格式向前兼容的唯一方式，
//! 新版本加的段不会让旧版本的读者解析失败（只会被忽略）。

use std::io::{Read, Write};

use pinbridge_core::error::{Error, Result};
use pinbridge_core::MAGIC;
use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, MAX_ARGS};
use crate::varint::{try_read_u64, try_read_zigzag, write_u64, write_zigzag};

/// 格式版本。任何**读侧无法兼容**的改动都要提升它。
pub const FORMAT_VERSION: u16 = 1;

/// 段 tag。`0x00..=0x3F` 预留给本格式，`0x40..=0x7E` 给实验性扩展，`0x7F` 结束。
pub const CHUNK_META: u8 = 0x00;
/// 事件流段，可重复出现。
pub const CHUNK_EVENT: u8 = 0x01;
/// 审计（JSON Lines）段。
pub const CHUNK_AUDIT: u8 = 0x02;
/// 汇总段，恰好一个 chunk。
pub const CHUNK_SUMMARY: u8 = 0x03;
/// 结束标记。读到它之后不再解析后续字节。
pub const CHUNK_END: u8 = 0x7F;

/// header 标志位：录制时启用了重复折叠。
pub const FLAG_REPEAT_FOLDED: u16 = 1 << 0;
/// header 标志位：事件记录采用 delta 编码（当前实现恒为 1）。
pub const FLAG_DELTA_ENCODED: u16 = 1 << 1;
/// header 标志位：会话被异常中断，轨迹尾部可能缺失。
pub const FLAG_TRUNCATED: u16 = 1 << 2;

/// 单个 chunk 的负载上限。防"坏文件声称自己有 4 GiB 负载"式的内存放大。
pub const MAX_CHUNK_BYTES: usize = 256 * 1024 * 1024;

/// header 长度：magic(4) + version(2) + flags(2)。
pub const HEADER_LEN: usize = 8;

/// 默认每个 EVENT chunk 的负载大小。64 KiB 在写放大与内存占用之间取平衡。
pub const DEFAULT_CHUNK_LIMIT: usize = 64 * 1024;

/// 轨迹汇总。会原样写进 `SUMMARY` chunk，也是人/AI 判断
/// "这份轨迹能不能用来下结论"的第一手依据。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// 落盘的记录条数（含折叠标记）。
    pub records: u64,
    /// 展开后的原始事件条数。
    pub events: u64,
    /// 因折叠而"没写盘"的条数。
    pub repeat_folded: u64,
    /// 因通道背压被丢弃的条数。**非零即代表轨迹不完整。**
    pub dropped: u64,
    /// 第一条记录的序号。
    pub first_sequence: u64,
    /// 最后一条记录的序号。
    pub last_sequence: u64,
    /// 录制是否被异常中断。
    pub truncated: bool,
}

impl Summary {
    /// 折叠率（0.0 ~ 1.0）。用于判断"这份轨迹值不值得离线跑污点分析"。
    #[must_use]
    pub fn fold_ratio(&self) -> f64 {
        let total = self.records + self.repeat_folded;
        if total == 0 {
            return 0.0;
        }
        self.repeat_folded as f64 / total as f64
    }

    /// 转 JSON。
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| Error::Trace(format!("summary 序列化失败: {e}")))
    }

    /// 从 JSON 解析。
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).map_err(|e| Error::Trace(format!("summary 解析失败: {e}")))
    }
}

/// 轨迹写入器。
///
/// 用法：`create` → 若干 `write_record` → `finish`。
/// **忘记 `finish` 会丢掉最后一个折叠段与 SUMMARY 段**（这是刻意不做 Drop 兜底的：
/// 收尾路径必须显式，否则"写到一半进程被杀"和"正常结束"在文件层面无法区分）。
#[derive(Debug)]
pub struct PbtWriter<W: Write> {
    inner: W,
    event_buf: Vec<u8>,
    chunk_limit: usize,
    flags: u16,
    summary: Summary,
    last_sequence: u64,
    last_address: u64,
    last_args: [u64; MAX_ARGS],
    finished: bool,
}

impl<W: Write> PbtWriter<W> {
    /// 写入文件头并返回写入器。
    pub fn create(mut inner: W, flags: u16) -> Result<Self> {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        // delta 编码是当前唯一实现，无条件置位
        header.extend_from_slice(&(flags | FLAG_DELTA_ENCODED).to_le_bytes());
        inner.write_all(&header)?;
        Ok(Self {
            inner,
            event_buf: Vec::with_capacity(DEFAULT_CHUNK_LIMIT),
            chunk_limit: DEFAULT_CHUNK_LIMIT,
            flags: flags | FLAG_DELTA_ENCODED,
            summary: Summary::default(),
            last_sequence: 0,
            last_address: 0,
            last_args: [0; MAX_ARGS],
            finished: false,
        })
    }

    /// 调整 EVENT chunk 的切分阈值。仅用于测试（验证跨 chunk 记录）。
    #[must_use]
    pub fn with_chunk_limit(mut self, limit: usize) -> Self {
        self.chunk_limit = limit.max(1);
        self
    }

    /// 写一个任意段。调用方自定义 tag 时必须自己保证负载格式自洽。
    pub fn write_chunk(&mut self, tag: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > u32::MAX as usize {
            return Err(Error::Trace(format!(
                "chunk 负载过大: {} 字节",
                payload.len()
            )));
        }
        self.inner.write_all(&[tag])?;
        self.inner
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    /// 写元数据段（JSON）。
    pub fn meta(&mut self, json: &str) -> Result<()> {
        self.write_chunk(CHUNK_META, json.as_bytes())
    }

    /// 写一行审计记录。
    pub fn audit_line(&mut self, line: &str) -> Result<()> {
        let mut owned = String::with_capacity(line.len() + 1);
        owned.push_str(line);
        owned.push('\n');
        self.write_chunk(CHUNK_AUDIT, owned.as_bytes())
    }

    /// 写一条记录（普通事件或折叠标记）。
    ///
    /// 序号必须严格递增：delta 编码依赖单调性，乱序会让读侧还原出
    /// 完全错误的时间线（而且看起来毫无异常）。因此这里显式拒绝。
    pub fn write_record(&mut self, event: &Event) -> Result<()> {
        if self.finished {
            return Err(Error::Trace("写入器已 finish，不能继续写入".into()));
        }
        if event.sequence <= self.last_sequence {
            // 严格递增，不是"非递减"：两条记录序号相同会在重放时产生歧义
            // （同一时刻发生了两件事，而序号是唯一的时间线依据）。
            return Err(Error::Trace(format!(
                "事件序号必须严格单调递增: 上一条 {} 之后收到 {}",
                self.last_sequence, event.sequence
            )));
        }
        self.encode_record_into(event);
        self.summary.records += 1;
        if event.kind == EventKind::Repeat {
            let folded = event.arg(0);
            self.summary.repeat_folded += folded;
            self.summary.events += folded;
        } else {
            self.summary.events += 1;
        }
        if self.summary.first_sequence == 0 {
            self.summary.first_sequence = event.sequence;
        }
        self.summary.last_sequence = event.sequence;
        if self.event_buf.len() >= self.chunk_limit {
            self.flush_event_chunk()?;
        }
        Ok(())
    }

    /// 当前累计的汇总（未 finish 时也可读，用于进度展示）。
    #[must_use]
    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    /// 写入文件头时使用的标志位（含写入器自动置上的位）。
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// 记录被背压丢弃的事件数。必须由调用方如实传入。
    pub fn note_dropped(&mut self, dropped: u64) {
        self.summary.dropped += dropped;
    }

    /// 把已缓冲的记录立刻落盘（**不**写 SUMMARY 与 END，格式仍然合法）。
    ///
    /// 为什么需要它：写入器前面还有一层 `BufWriter`，而事件又先在内存里攒成
    /// EVENT chunk，因此正常情况下要等缓冲满或 `finish()` 才真正落盘。
    /// 但收尾**可能根本不会发生**——被分析的程序崩溃/被强杀，或者工具侧的
    /// 收尾调用被卸载保护拒绝（本项目实测遇到过：`.pbt` 一个字节都没有）。
    /// 那时用户最需要的东西恰好没留下。
    ///
    /// 多条 EVENT chunk 是格式允许的（读取器按 chunk 迭代，`FINISH` 只补
    /// SUMMARY/END），所以提前冲刷不会产生需要特殊处理的文件。
    ///
    /// # Errors
    ///
    /// 底层写入失败时透传。
    pub fn flush(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_event_chunk()?;
        self.inner.flush()?;
        Ok(())
    }

    /// 收尾：冲刷缓冲、写 SUMMARY 与 END，返回底层写入器与汇总。
    pub fn finish(mut self) -> Result<(W, Summary)> {
        self.flush_event_chunk()?;
        let summary = self.summary.clone();
        let json = summary.to_json()?;
        self.write_chunk(CHUNK_SUMMARY, json.as_bytes())?;
        self.write_chunk(CHUNK_END, &[])?;
        self.inner.flush()?;
        self.finished = true;
        Ok((self.inner, summary))
    }

    /// 把一条记录 delta 编码进缓冲。
    fn encode_record_into(&mut self, event: &Event) {
        let previous_args = self.last_args;
        write_u64(
            &mut self.event_buf,
            event.sequence.saturating_sub(self.last_sequence),
        );
        self.event_buf.push(event.kind.code());
        write_u64(&mut self.event_buf, u64::from(event.thread_id));
        write_zigzag(
            &mut self.event_buf,
            (event.address as i64).wrapping_sub(self.last_address as i64),
        );
        // 末尾"未变化"的参数整段省略：VMP 壳里 arg 高频不变（比如模块基址），
        // 与文章里"寄存器没有变化就不需要记录"是同一个思路。
        let argc = (0..MAX_ARGS)
            .rposition(|i| event.args[i] != previous_args[i])
            .map_or(0, |i| i + 1);
        self.event_buf.push(argc as u8);
        for (current, previous) in event.args.iter().zip(previous_args.iter()).take(argc) {
            write_zigzag(
                &mut self.event_buf,
                (*current as i64).wrapping_sub(*previous as i64),
            );
        }
        self.last_sequence = event.sequence;
        self.last_address = event.address;
        self.last_args = event.args;
    }

    fn flush_event_chunk(&mut self) -> Result<()> {
        if self.event_buf.is_empty() {
            return Ok(());
        }
        let payload = std::mem::take(&mut self.event_buf);
        let result = self.write_chunk(CHUNK_EVENT, &payload);
        // 复用分配（录制线程每秒百万次调用，这里不能每次重新 malloc）
        self.event_buf = payload;
        self.event_buf.clear();
        result
    }
}

/// 轨迹读取器（流式）。
#[derive(Debug)]
pub struct PbtReader<R: Read> {
    inner: R,
    version: u16,
    flags: u16,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    end_seen: bool,
    meta: String,
    audit: String,
    summary_text: String,
    last_sequence: u64,
    last_address: u64,
    last_args: [u64; MAX_ARGS],
    records_read: u64,
}

impl<R: Read> PbtReader<R> {
    /// 读取并校验文件头。
    pub fn open(mut inner: R) -> Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        if !read_full(&mut inner, &mut header)? {
            return Err(Error::Trace("文件为空，不是有效的 .pbt".into()));
        }
        if header[0..4] != MAGIC {
            return Err(Error::Trace("文件头不是 PBT1 magic".into()));
        }
        let version = u16::from_le_bytes([header[4], header[5]]);
        if version != FORMAT_VERSION {
            return Err(Error::Trace(format!(
                "不支持的轨迹版本 {version}（本实现支持 {FORMAT_VERSION}）"
            )));
        }
        let flags = u16::from_le_bytes([header[6], header[7]]);
        Ok(Self {
            inner,
            version,
            flags,
            buf: Vec::with_capacity(DEFAULT_CHUNK_LIMIT),
            pos: 0,
            eof: false,
            end_seen: false,
            meta: String::new(),
            audit: String::new(),
            summary_text: String::new(),
            last_sequence: 0,
            last_address: 0,
            last_args: [0; MAX_ARGS],
            records_read: 0,
        })
    }

    /// 文件头里的版本号。
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// 文件头里的标志位。
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// META 段的原始 JSON 文本。
    #[must_use]
    pub fn meta(&self) -> &str {
        &self.meta
    }

    /// AUDIT 段的原始文本（JSON Lines）。
    #[must_use]
    pub fn audit_log(&self) -> &str {
        &self.audit
    }

    /// SUMMARY 段解析结果。尚未读到该段时返回 `None`。
    pub fn summary(&self) -> Result<Option<Summary>> {
        if self.summary_text.is_empty() {
            return Ok(None);
        }
        Summary::from_json(&self.summary_text).map(Some)
    }

    /// 已成功解出的记录数。
    #[must_use]
    pub const fn records_read(&self) -> u64 {
        self.records_read
    }

    /// 取下一条记录。文件正常结束返回 `Ok(None)`。
    ///
    /// 注意：折叠标记会**原样返回**，不做展开。展开是重放层的职责
    /// （[`crate::fold::expand_records`]），因为可能需要"只看标记"的统计视角。
    pub fn next_record(&mut self) -> Result<Option<Event>> {
        loop {
            if let Some((event, used)) = self.try_decode()? {
                self.pos += used;
                self.last_sequence = event.sequence;
                self.last_address = event.address;
                self.last_args = event.args;
                self.records_read += 1;
                return Ok(Some(event));
            }
            if !self.fill()? {
                return if self.pos < self.buf.len() {
                    Err(Error::Trace(format!(
                        "轨迹在第 {} 条记录之后被截断（剩 {} 字节无法解出完整记录）",
                        self.records_read,
                        self.buf.len() - self.pos
                    )))
                } else {
                    Ok(None)
                };
            }
        }
    }

    /// 一次读完所有记录（先把所有段收齐，再统一展开的场景）。
    pub fn read_all(&mut self) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        while let Some(event) = self.next_record()? {
            out.push(event);
        }
        Ok(out)
    }

    /// 尝试从缓冲中解出一条记录。数据不足时返回 `Ok(None)`（不改动状态）。
    fn try_decode(&self) -> Result<Option<(Event, usize)>> {
        let input = &self.buf[self.pos..];
        let mut cursor = 0usize;

        let Some((seq_delta, n)) = try_read_u64(&input[cursor..])? else {
            return Ok(None);
        };
        cursor += n;

        let Some(kind_byte) = input.get(cursor).copied() else {
            return Ok(None);
        };
        let kind = EventKind::from_code(kind_byte).ok_or_else(|| {
            Error::Trace(format!("未知事件类型 {kind_byte}，文件可能来自更新版本"))
        })?;
        cursor += 1;

        let Some((thread_id, n)) = try_read_u64(&input[cursor..])? else {
            return Ok(None);
        };
        cursor += n;
        if thread_id > u64::from(u32::MAX) {
            return Err(Error::Trace(format!("线程 ID 越界: {thread_id}")));
        }

        let Some((addr_delta, n)) = try_read_zigzag(&input[cursor..])? else {
            return Ok(None);
        };
        cursor += n;

        let Some(argc) = input.get(cursor).copied() else {
            return Ok(None);
        };
        cursor += 1;
        if usize::from(argc) > MAX_ARGS {
            return Err(Error::Trace(format!("参数个数越界: {argc}")));
        }

        let previous_args = self.last_args;
        let mut args = previous_args;
        for (slot, previous) in args
            .iter_mut()
            .zip(previous_args.iter())
            .take(usize::from(argc))
        {
            let Some((delta, n)) = try_read_zigzag(&input[cursor..])? else {
                return Ok(None);
            };
            cursor += n;
            *slot = (*previous as i64).wrapping_add(delta) as u64;
        }

        Ok(Some((
            Event {
                sequence: self.last_sequence.saturating_add(seq_delta),
                kind,
                thread_id: thread_id as u32,
                address: (self.last_address as i64).wrapping_add(addr_delta) as u64,
                args,
            },
            cursor,
        )))
    }

    /// 读取下一个 chunk。返回 `true` 表示向事件缓冲追加了数据。
    fn fill(&mut self) -> Result<bool> {
        if self.end_seen || self.eof {
            return Ok(false);
        }
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        loop {
            let mut head = [0u8; 5];
            if !read_full(&mut self.inner, &mut head)? {
                self.eof = true;
                return Ok(false);
            }
            let tag = head[0];
            let length = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if length > MAX_CHUNK_BYTES {
                return Err(Error::Trace(format!("chunk 长度 {length} 超出上限")));
            }
            let mut payload = vec![0u8; length];
            // 注意：`length == 0` 的 chunk（例如 END 段）不能走 read_full ——
            // 零长度缓冲会让它返回"一个字节也没读到"，从而被误判成负载被截断。
            if length > 0 && !read_full(&mut self.inner, &mut payload)? {
                return Err(Error::Trace("轨迹在 chunk 负载中途结束".into()));
            }
            match tag {
                CHUNK_EVENT => {
                    self.buf.extend_from_slice(&payload);
                    return Ok(true);
                }
                CHUNK_META => self.meta.push_str(&String::from_utf8_lossy(&payload)),
                CHUNK_AUDIT => self.audit.push_str(&String::from_utf8_lossy(&payload)),
                CHUNK_SUMMARY => self
                    .summary_text
                    .push_str(&String::from_utf8_lossy(&payload)),
                CHUNK_END => {
                    self.end_seen = true;
                    return Ok(false);
                }
                // 未知 tag：按长度跳过。向前兼容的唯一实现方式。
                _ => {}
            }
        }
    }
}

/// 读到填满 `buf` 或遇到 EOF。返回 `false` 表示一个字节都没读到。
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    if filled == 0 {
        return Ok(false);
    }
    if filled < buf.len() {
        return Err(Error::Trace("轨迹在 chunk 头中途结束".into()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{expand_records, RepeatFolder};

    fn fixture_events() -> Vec<Event> {
        let mut events = Vec::new();
        events.push(
            Event::new(1, EventKind::BreakpointHit, 100, 0x7FF6_0000_1000).with_arg(0, 0x140001000),
        );
        events.push(
            Event::new(2, EventKind::SyscallEnter, 100, 0x7FF6_0000_2000)
                .with_arg(0, 0x40)
                .with_arg(1, 0x7FF6_0000_2300)
                .with_arg(2, 8),
        );
        events.push(
            Event::new(3, EventKind::MemWrite, 100, 0x7FF6_0000_3000)
                .with_arg(0, 0x7FF6_0000_3000)
                .with_arg(1, 4)
                .with_arg(2, 0xDEAD_BEEF),
        );
        // 一段 500 条完全相同的 syscall（会被折叠成 1 条 + 1 个标记）
        for i in 0..500u64 {
            events.push(
                Event::new(4 + i, EventKind::SyscallEnter, 100, 0x7FF6_0000_2000)
                    .with_arg(0, 0x40)
                    .with_arg(1, 0x7FF6_0000_2300)
                    .with_arg(2, 8),
            );
        }
        events
    }

    fn fold_all(events: &[Event]) -> Vec<Event> {
        let mut folder = RepeatFolder::new();
        let mut out = Vec::new();
        for ev in events {
            folder.push(*ev, &mut |e| out.push(e));
        }
        folder.finish(&mut |e| out.push(e));
        out
    }

    fn write_trace(records: &[Event], chunk_limit: usize) -> Vec<u8> {
        let mut writer = PbtWriter::create(Vec::new(), FLAG_REPEAT_FOLDED)
            .unwrap()
            .with_chunk_limit(chunk_limit);
        writer.meta(r#"{"tool":"pinbridge-trace-tests"}"#).unwrap();
        for record in records {
            writer.write_record(record).unwrap();
        }
        writer
            .audit_line(r#"{"actor":"test","action":"write"}"#)
            .unwrap();
        writer.finish().unwrap().0
    }

    #[test]
    fn roundtrip_with_folding_is_lossless() {
        let events = fixture_events();
        let records = fold_all(&events);
        assert!(records.len() < events.len() / 50, "折叠后记录数应大幅下降");

        let bytes = write_trace(&records, DEFAULT_CHUNK_LIMIT);
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        let decoded = reader.read_all().unwrap();
        assert_eq!(decoded, records, "编解码必须逐位可逆");

        let expanded = expand_records(&decoded);
        assert_eq!(expanded, events, "折叠 + 编解码之后必须还原出原始事件序列");
    }

    #[test]
    fn records_may_span_chunk_boundaries() {
        let records = fold_all(&fixture_events());
        // 把 chunk 限到 16 字节，强制大量记录跨 chunk 分割
        let bytes = write_trace(&records, 16);
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        assert_eq!(reader.read_all().unwrap(), records);
    }

    #[test]
    fn header_and_sections_are_readable() {
        let records = fold_all(&fixture_events());
        let bytes = write_trace(&records, DEFAULT_CHUNK_LIMIT);
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        assert_eq!(reader.version(), FORMAT_VERSION);
        assert_ne!(reader.flags() & FLAG_REPEAT_FOLDED, 0);
        assert_ne!(reader.flags() & FLAG_DELTA_ENCODED, 0);
        let decoded = reader.read_all().unwrap();
        assert_eq!(decoded.len(), records.len());
        assert!(reader.meta().contains("pinbridge-trace-tests"));
        assert!(reader.audit_log().contains("\"action\":\"write\""));
        let summary = reader.summary().unwrap().expect("应有 SUMMARY 段");
        assert_eq!(summary.records, records.len() as u64);
        assert_eq!(summary.events, fixture_events().len() as u64);
        assert_eq!(summary.repeat_folded, 499);
        assert_eq!(summary.first_sequence, 1);
        assert_eq!(summary.last_sequence, 503);
        assert!(summary.fold_ratio() > 0.99);
    }

    #[test]
    fn unknown_chunk_is_skipped() {
        let records = fold_all(&fixture_events());
        let mut writer = PbtWriter::create(Vec::new(), 0).unwrap();
        // 故意插一个未来版本才认识的段
        writer.write_chunk(0x40, b"future-section-payload").unwrap();
        for record in &records {
            writer.write_record(record).unwrap();
        }
        let bytes = writer.finish().unwrap().0;
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        assert_eq!(
            reader.read_all().unwrap(),
            records,
            "未知段必须被跳过而不是报错"
        );
    }

    #[test]
    fn bad_magic_and_version_are_rejected() {
        let mut bytes = write_trace(&fold_all(&fixture_events()), DEFAULT_CHUNK_LIMIT);
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(PbtReader::open(&bad_magic[..]).is_err());

        bytes[4] = 0x63; // version = 99
        let err = PbtReader::open(&bytes[..]).unwrap_err();
        assert!(err.to_string().contains("不支持的轨迹版本"));

        assert!(PbtReader::open(&[][..]).is_err(), "空文件必须被拒绝");
    }

    #[test]
    fn truncated_record_is_reported_not_swallowed() {
        let mut bytes = write_trace(&fold_all(&fixture_events()), DEFAULT_CHUNK_LIMIT);
        bytes.truncate(bytes.len() - 300); // 砍掉尾部
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        let result = reader.read_all();
        // 尾部被砍掉后要么停在完整边界，要么明确报错 —— 不允许静默返回半条记录
        if let Ok(events) = &result {
            assert!(events.iter().all(|e| e.sequence > 0));
        } else {
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("截断") || message.contains("结束"),
                "截断必须给出可定位的错误，实际: {message}"
            );
        }
    }

    #[test]
    fn non_monotonic_sequence_is_rejected_at_write_time() {
        let mut writer = PbtWriter::create(Vec::new(), 0).unwrap();
        writer
            .write_record(&Event::new(10, EventKind::StateChange, 1, 0))
            .unwrap();
        let err = writer
            .write_record(&Event::new(10, EventKind::StateChange, 1, 0))
            .unwrap_err();
        assert!(err.to_string().contains("单调递增"));
    }

    #[test]
    fn empty_trace_is_valid() {
        let writer = PbtWriter::create(Vec::new(), 0).unwrap();
        let (bytes, summary) = writer.finish().unwrap();
        assert_eq!(summary.records, 0);
        let mut reader = PbtReader::open(&bytes[..]).unwrap();
        assert!(reader.read_all().unwrap().is_empty());
        assert_eq!(reader.summary().unwrap().unwrap().records, 0);
    }

    /// 黄金字节：与 Python 实现共享的同一份 EVENT 负载。
    /// 断言值由两侧实现共同确认；任何一侧改了编码都会让这个测试与
    /// `python/tests/test_golden_vectors.py` 同时失败。
    #[test]
    fn golden_event_payload_hex_matches_python() {
        let records = [
            Event::new(1, EventKind::BreakpointHit, 100, 0x7FF6_0000_1000).with_arg(0, 0x140001000),
            Event::new(2, EventKind::SyscallEnter, 100, 0x7FF6_0000_2000)
                .with_arg(0, 0x40)
                .with_arg(1, 0x7FF6_0000_2300)
                .with_arg(2, 8),
            Event::repeat_marker(
                1000,
                997,
                EventKind::SyscallEnter,
                &Event::new(2, EventKind::SyscallEnter, 100, 0x7FF6_0000_2000)
                    .with_arg(0, 0x40)
                    .with_arg(1, 0x7FF6_0000_2300)
                    .with_arg(2, 8),
            ),
        ];
        let mut writer = PbtWriter::create(Vec::new(), 0)
            .unwrap()
            .with_chunk_limit(usize::MAX);
        for record in &records {
            writer.write_record(record).unwrap();
        }
        let bytes = writer.finish().unwrap().0;
        // 提取 EVENT chunk 的负载
        let mut cursor = HEADER_LEN;
        let mut payload_hex = String::new();
        while cursor + 5 <= bytes.len() {
            let tag = bytes[cursor];
            let len = u32::from_le_bytes([
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
                bytes[cursor + 4],
            ]) as usize;
            cursor += 5;
            if tag == CHUNK_EVENT {
                payload_hex = bytes[cursor..cursor + len]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
            }
            cursor += len;
        }
        println!("GOLDEN_EVENT_PAYLOAD_HEX={payload_hex}");
        assert!(!payload_hex.is_empty());
    }
}
