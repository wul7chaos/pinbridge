//! 行分隔 JSON 编解码。
//!
//! 选择"一行一个 JSON 对象"而不是长度前缀，是因为：
//!
//! * 本机回环链路上，用 `nc` / `socat` / 手写 Python 就能直接对话，排查协议问题
//!   不需要额外工具。这在"AI 报了奇怪的错、人类要现场复核"的场景里非常值钱。
//! * 行分隔天然可流式，不需要等待整个缓冲区。
//!
//! 代价是必须显式限制单帧大小，否则一条坏包就能把 Hub 的内存打满。

use serde::de::DeserializeOwned;
use serde::Serialize;

use pinbridge_core::error::{Error, Result};

/// 单帧上限（字节）。超过即视为协议错误并断开连接。
///
/// 取值理由：最大的正常帧是 `ExportTrace` 的响应摘要与 `ReadMemory` 结果
/// （默认切片 64 KiB，base64 后 ~88 KiB）。留 4 倍余量。
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// 把值编码成一行 JSON（含结尾 `\n`）。
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut body =
        serde_json::to_vec(value).map_err(|e| Error::Protocol(format!("序列化失败: {e}")))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!(
            "帧过大: {} 字节 > 上限 {MAX_FRAME_BYTES}",
            body.len()
        )));
    }
    body.push(b'\n');
    Ok(body)
}

/// 把一行 JSON 解码成目标类型。会先做长度检查再解析。
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!(
            "帧过大: {} 字节 > 上限 {MAX_FRAME_BYTES}",
            trimmed.len()
        )));
    }
    serde_json::from_str(trimmed).map_err(|e| Error::Protocol(format!("反序列化失败: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::Request;

    #[test]
    fn roundtrip_request() {
        let req = Request::Detach { session: 3 };
        let bytes = encode_line(&req).unwrap();
        assert_eq!(*bytes.last().unwrap(), b'\n');
        let text = String::from_utf8(bytes).unwrap();
        let back: Request = decode_line(&text).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn oversized_frame_is_rejected_before_parse() {
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        let err = decode_line::<Request>(&huge).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn crlf_is_tolerated() {
        let text = "{\"op\":\"detach\",\"session\":1}\r\n";
        let req: Request = decode_line(text).unwrap();
        assert_eq!(req, Request::Detach { session: 1 });
    }

    #[test]
    fn unknown_op_is_a_protocol_error() {
        let err = decode_line::<Request>("{\"op\":\"launch_missile\"}").unwrap_err();
        assert!(err.to_string().contains("反序列化失败"));
    }
}
