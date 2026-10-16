//! 传输抽象。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use pinbridge_core::error::{Error, Result};

/// 一次请求/响应往返的传输层。
///
/// 约定：**一次 `roundtrip` 只对应一个请求**。不做流水线复用，
/// 因为 agent 侧的处理必须在目标线程的上下文里完成（见
/// `docs/design/script-runtime.md`），流水线会让"这条命令到底在哪个
/// 执行点上生效"变得不可证。
pub trait Transport: std::fmt::Debug {
    /// 发送请求，等待响应。`timeout` 由调用方按命令类型决定
    /// （读一个字节与单步执行整个进程的超时完全不是一回事）。
    fn roundtrip(&mut self, request: &[u8], timeout: Duration) -> Result<Vec<u8>>;

    /// 传输层的人类可读描述（出现在错误信息与审计里）。
    fn describe(&self) -> String;
}

/// 回环 TCP 传输。帧格式：`u32` 小端长度 + 负载。
///
/// 长度前缀在这里是必要的：agent 侧的响应可能包含内存切片，
/// 而回环 TCP 不保证一次 `read` 就能拿到完整帧。
#[derive(Debug)]
pub struct TcpTransport {
    stream: TcpStream,
    peer: SocketAddr,
}

impl TcpTransport {
    /// 连接到 agent 上报的回环地址。
    ///
    /// 会**强制**校验对端是回环地址：这是硬约束，不接受任何绕过。
    pub fn connect(addr: impl ToSocketAddrs) -> Result<Self> {
        let peer = addr
            .to_socket_addrs()
            .map_err(Error::Io)?
            .next()
            .ok_or_else(|| Error::TargetUnavailable("无法解析 agent 地址".into()))?;
        if !peer.ip().is_loopback() {
            return Err(Error::TargetUnavailable(format!(
                "拒绝连接非回环地址 {peer}：agent 链路只允许本机通信"
            )));
        }
        let stream = TcpStream::connect(peer)?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, peer })
    }

    /// 对端地址。
    #[must_use]
    pub const fn peer(&self) -> SocketAddr {
        self.peer
    }
}

impl Transport for TcpTransport {
    fn roundtrip(&mut self, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        let len = u32::try_from(request.len()).map_err(|_| Error::Protocol("请求帧过大".into()))?;
        self.stream.write_all(&len.to_le_bytes())?;
        self.stream.write_all(request)?;
        self.stream.flush()?;

        let mut head = [0u8; 4];
        self.stream.read_exact(&mut head)?;
        let payload_len = u32::from_le_bytes(head) as usize;
        if payload_len > pinbridge_proto::MAX_FRAME_BYTES {
            return Err(Error::Protocol(format!("响应帧过大: {payload_len} 字节")));
        }
        let mut payload = vec![0u8; payload_len];
        self.stream.read_exact(&mut payload)?;
        Ok(payload)
    }

    fn describe(&self) -> String {
        format!("tcp://{}", self.peer)
    }
}

/// 内存传输：单测与"无目标进程"演示用。
///
/// `scripted` 里的响应对按 FIFO 匹配请求；用完之后返回空响应
/// （而不是报错），这样"只想验证请求序列"的测试不需要为每个请求准备应答。
#[derive(Debug, Default)]
pub struct MemoryTransport {
    scripted: VecDeque<Vec<u8>>,
    sent: Vec<Vec<u8>>,
}

impl MemoryTransport {
    /// 新建，带预设响应。
    #[must_use]
    pub fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            scripted: responses.into_iter().collect(),
            sent: Vec::new(),
        }
    }

    /// 已发送的请求（按顺序），供测试断言。
    #[must_use]
    pub fn sent(&self) -> &[Vec<u8>] {
        &self.sent
    }

    /// 剩余未消费的预设响应数。
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.scripted.len()
    }
}

impl Transport for MemoryTransport {
    fn roundtrip(&mut self, request: &[u8], _timeout: Duration) -> Result<Vec<u8>> {
        self.sent.push(request.to_vec());
        Ok(self.scripted.pop_front().unwrap_or_default())
    }

    fn describe(&self) -> String {
        format!("memory://(scripted={})", self.scripted.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn memory_transport_records_requests_in_order() {
        let mut t = MemoryTransport::new([b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(t.roundtrip(b"a", Duration::from_millis(1)).unwrap(), b"one");
        assert_eq!(t.roundtrip(b"b", Duration::from_millis(1)).unwrap(), b"two");
        assert_eq!(
            t.roundtrip(b"c", Duration::from_millis(1)).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(t.sent().len(), 3);
        assert_eq!(t.remaining(), 0);
        assert!(t.describe().starts_with("memory://"));
    }

    #[test]
    fn non_loopback_addresses_are_refused() {
        let err = TcpTransport::connect("8.8.8.8:1").unwrap_err();
        assert!(err.to_string().contains("非回环地址"), "实际: {err}");
    }

    #[test]
    fn loopback_detection_covers_127_range_and_v6() {
        assert!(Ipv4Addr::LOCALHOST.is_loopback());
        assert!(IpAddr::V4(Ipv4Addr::new(127, 8, 8, 8)).is_loopback());
        assert!(!IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)).is_loopback());
        assert!(std::net::Ipv6Addr::LOCALHOST.is_loopback());
    }
}
