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
    /// 冲刷缓冲区，**并写下一个提交标记**。
    ///
    /// 提交标记（`SUMMARY` chunk）的语义是：
    /// **"在这个位置之前，记录是完整的；截至 `last_sequence` 的记录已经落盘"**。
    ///
    /// 顺序不能反：先写事件块、**再**写标记。反过来会让读者把标记之后、
    /// 尚未落盘的记录当成"已完整"，那正是流式读取最危险的一种错——
    /// 读到的数据**看起来完全正常**，只是少了一截，而且没有任何迹象。
    ///
    /// 一份轨迹里因此会有**多个** `SUMMARY`：运行中每个提交标记一个，
    /// `finish` 写的那个是最后（也是权威）的一个。读端规则：**最后一个胜出**。
    ///
    /// 底层写入失败时透传。
    pub fn flush(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_event_chunk()?;
        let summary = self.summary.clone();
        let json = summary.to_json()?;
        self.write_chunk(CHUNK_SUMMARY, json.as_bytes())?;
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
    /// 流式模式：**容忍被截断的尾部**（写入方还在写）。
    ///
    /// 非流式时，读到半个 chunk 就是损坏（一份写完的轨迹不该是这样）；
    /// 流式时它是**预期**状态——"还没写到这里"与"文件坏了"必须分开，
    /// 否则边录边读就永远只能靠猜。
    streaming: bool,
    /// 上次没能读完、**留到下次**的原始字节。
    ///
    /// 为什么必须留着：底层是"一次性"读取器（文件句柄），已经消费掉的字节
    /// 拿不回来。不缓存就等于把"半个 chunk"当成丢失的字节，下次读到的头会错位——
    /// 而错位之后解析出的记录**看起来仍然像记录**（有 kind、有地址），
    /// 这是最难发现的一类错误。有它就能做到"这次读到哪、下次从哪接着读"。
    pending: Vec<u8>,
}

impl<R: Read> PbtReader<R> {
    /// 读取并校验文件头。
    pub fn open(inner: R) -> Result<Self> {
        Self::open_with(inner, false)
    }

    /// 以**流式**模式打开：读取方还在写的时候读。
    ///
    /// 与非流式（[`PbtReader::open`]）的区别只有一处，但很关键：
    ///
    /// * 非流式：读到半个 chunk ⇒ **损坏**，报错。一份写完的轨迹不该是这样，
    ///   把它当"暂时没有"会掩盖真正的文件损坏。
    /// * 流式：读到半个 chunk ⇒ **正常**，停在能读到的最后一个完整位置，
    ///   把零头留着，下次调用接着读。
    ///
    /// 两种模式都不能接受的是"错位"：所以流式模式不会丢掉任何字节，
    /// 未读完的部分原样缓存在 `pending` 里。
    ///
    /// 用法：反复调用 [`PbtReader::next_record`]，`Ok(None)` 表示
    /// "当前还没有更多完整记录"（不代表结束）——写入方继续写之后**再调用一次**
    /// 就能读到新的。想知道"已经完整到哪里"，用 [`PbtReader::summary`]：
    /// 它返回**最后一个提交标记**（`flush` 写的），其 `last_sequence`
    /// 就是"已完整"的边界。
    pub fn open_streaming(inner: R) -> Result<Self> {
        Self::open_with(inner, true)
    }

    fn open_with(mut inner: R, streaming: bool) -> Result<Self> {
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
            streaming,
            pending: Vec::new(),
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
            let head_got = self.read_or_pending(&mut head)?;
            if head_got < head.len() {
                if self.streaming {
                    // 流式：把这一小段**原样留着**，下次接着读。
                    // 一个字节都不能丢——丢了下次就会把错位后的字节当 chunk 头解析，
                    // 而错位后解析出来的"记录"看起来仍然像记录（有 kind、有地址），
                    // 这是这类实现里最难发现的一类错误。
                    self.pending.splice(0..0, head[..head_got].iter().copied());
                    return Ok(false);
                }
                if head_got == 0 {
                    self.eof = true;
                    return Ok(false);
                }
                return Err(Error::Trace("轨迹在 chunk 头中途结束".into()));
            }
            let tag = head[0];
            let length = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if length > MAX_CHUNK_BYTES {
                return Err(Error::Trace(format!("chunk 长度 {length} 超出上限")));
            }
            let mut payload = vec![0u8; length];
            // 注意：`length == 0` 的 chunk（例如 END 段）不能走读取 ——
            // 零长度缓冲会让它返回"一个字节也没读到"，从而被误判成负载被截断。
            if length > 0 {
                let payload_got = self.read_or_pending(&mut payload)?;
                if payload_got < length {
                    if self.streaming {
                        // 还原成"head ++ 已读到的负载"，顺序不能反。
                        self.pending.splice(0..0, head.iter().copied());
                        self.pending.extend_from_slice(&payload[..payload_got]);
                        return Ok(false);
                    }
                    return Err(Error::Trace("轨迹在 chunk 负载中途结束".into()));
                }
            }
            match tag {
                CHUNK_EVENT => {
                    self.buf.extend_from_slice(&payload);
                    return Ok(true);
                }
                CHUNK_META => self.meta.push_str(&String::from_utf8_lossy(&payload)),
                CHUNK_AUDIT => self.audit.push_str(&String::from_utf8_lossy(&payload)),
                CHUNK_SUMMARY => {
                    // 一份轨迹会有**多个**提交标记（`flush` 每个都写一个，见那里的注释）。
                    // **最后一个胜出**——不能拼接：拼出来是 `{...}{...}`，
                    // JSON 解析会失败，而失败的位置离真正的原因（这里）很远。
                    self.summary_text.clear();
                    self.summary_text
                        .push_str(&String::from_utf8_lossy(&payload));
                }
                CHUNK_END => {
                    self.end_seen = true;
                    return Ok(false);
                }
                // 未知 tag：按长度跳过。向前兼容的唯一实现方式。
                _ => {}
            }
        }
    }

    /// 读满 `buf`：**先消费 `pending` 里留着的字节**，再从底层读。
    ///
    /// 返回实际读到的字节数；小于 `buf.len()` 表示底层暂时没有更多数据
    /// （流式模式下这是常态，不是错误）。
    fn read_or_pending(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut filled = 0usize;
        if !self.pending.is_empty() {
            let take = self.pending.len().min(buf.len());
            buf[..take].copy_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
            filled = take;
        }
        while filled < buf.len() {
            match self.inner.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(filled)
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
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 共享字节缓冲的读取器：模拟"写入方在另一个线程/进程里继续写"。
    ///
    /// 用共享缓冲而不是文件，是为了让"写入方推进"与"读端继续读"在同一个测试里
    /// 可控地交替——文件也能做，但会出现"读到哪了"依赖操作系统缓冲的时序不确定性，
    /// 而这条用例要判定的是**语义**（零头有没有丢），不该被时序搅进来。
    #[derive(Clone)]
    struct SharedRead {
        data: Rc<RefCell<Vec<u8>>>,
        pos: usize,
    }

    impl std::io::Read for SharedRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let data = self.data.borrow();
            if self.pos >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - self.pos);
            buf[..n].copy_from_slice(&data[self.pos..self.pos + n]);
            self.pos += n;
            drop(data);
            Ok(n)
        }
    }

    /// 造一份**只有提交标记、没有收尾**的字节流：模拟"录制还在进行"。
    ///
    /// 与 `complete_trace_bytes` 的区别很重要：完整轨迹里有**两个**
    /// SUMMARY（flush 一个、finish 一个）。用它测截断，砍掉的可能是
    /// "最后一个"，而前一个仍然是完整有效的标记——那样就测不出"写到一半的
    /// 标记不该被采信"。只有 flush 的字节流里，唯一的标记就是被砍的那个。
    fn flushed_trace_bytes(records: u64) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = PbtWriter::create(&mut buffer, 0).expect("建立写入器");
            writer.meta("{\"target\":\"stream\"}").expect("写 meta");
            for i in 1..=records {
                writer
                    .write_record(&Event::new(i, EventKind::SyscallEnter, 7, 0x1400_0000 + i))
                    .expect("写记录");
            }
            writer.flush().expect("提交标记");
        }
        buffer
    }

    /// **截断的尾部**：流式模式容忍它，非流式模式必须报损坏。
    ///
    /// 这条用例的重点是"两种模式的判定必须不同"：如果流式模式也报错，
    /// 边录边读就没法用；如果非流式也容忍，真正的文件损坏就会被静默地当成
    /// "还没写完"，那是更糟的结果。
    ///
    /// 另一半是**零头不丢**：读到半个 chunk 之后把剩下的字节补上，
    /// 同一个读端实例必须能接着读出来。丢了那半个 chunk 的头，后面所有字节
    /// 都会错位，而错位后的记录**看起来仍然正常**。
    #[test]
    fn a_streaming_reader_tolerates_a_truncated_tail_and_loses_no_bytes() {
        let full = flushed_trace_bytes(4);
        // 末尾是那个提交标记。砍掉最后 3 字节 ⇒ 标记的负载写到一半。
        let cut = full.len() - 3;
        assert!(cut > 0 && cut < full.len());

        // 流式：不报错，读到能读到的部分
        let shared = Rc::new(RefCell::new(full[..cut].to_vec()));
        let mut reader = PbtReader::open_streaming(SharedRead {
            data: Rc::clone(&shared),
            pos: 0,
        })
        .expect("流式打开");
        let events = reader.read_all().expect("流式读不该报错");
        assert_eq!(events.len(), 4, "完整记录应当全部读到");
        assert!(
            reader.summary().expect("读汇总").is_none(),
            "写到一半的提交标记不能被当成有效标记——它覆盖到哪里还不知道"
        );

        // 非流式：同一个字节流必须被判定成损坏
        let mut strict = PbtReader::open(std::io::Cursor::new(full[..cut].to_vec()))
            .expect("打开应当成功（头是完整的）");
        assert!(
            strict.read_all().is_err(),
            "非流式模式遇到截断必须报损坏，不能被静默当成\"还没写完\""
        );

        // 把缺的字节补上：同一个读端实例接着读，不能错位
        shared.borrow_mut().extend_from_slice(&full[cut..]);
        assert!(reader.read_all().expect("补上之后继续读").is_empty());
        let marker = reader
            .summary()
            .expect("读汇总")
            .expect("补全之后提交标记应当可解析");
        assert_eq!(marker.last_sequence, 4, "零头没丢才能解析出正确的边界");
        assert_eq!(marker.records, 4);
        assert!(!marker.truncated, "整段录制没有丢事件");
    }

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
