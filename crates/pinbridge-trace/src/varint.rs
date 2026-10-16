//! varint / zigzag 编码。
//!
//! 选择 LEB128 而不是定长：事件里的 `address` 是 64 位，但绝大多数地址
//! 的高位在一段区间内不变，配合 delta 后 1~3 字节即可表示；
//! `address` 定长写 8 字节在亿级事件下就是浪费几百 MB。
//!
//! 选择 zigzag 而不是"符号 + 绝对值"：内存读写地址是双向跳变的
//! （`+0x40` 与 `-0x40` 一样常见），zigzag 让小负数的编码同样只有 1 字节。

use pinbridge_core::error::{Error, Result};

/// 写入无符号 LEB128。
pub fn write_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// 把 `i64` zigzag 映射为 `u64` 再写 varint。
pub fn write_zigzag(out: &mut Vec<u8>, value: i64) {
    let zz = ((value << 1) ^ (value >> 63)) as u64;
    write_u64(out, zz);
}

/// 从字节切片读取无符号 LEB128，返回 `(值, 消耗字节数)`。
///
/// 拒绝超过 10 字节的变长整数（防畸形输入导致的无限推进），
/// 这是解码器唯一必须防御的地方——轨迹文件是"外部输入"，
/// 可能来自别处（同事、CI、issue 附件）。
pub fn read_u64(input: &[u8]) -> Result<(u64, usize)> {
    try_read_u64(input)?.ok_or_else(|| Error::Trace("varint 在数据末尾被截断".into()))
}

/// 增量解码版本：数据不足以构成一个完整 varint 时返回 `Ok(None)`。
///
/// 流式读取 .pbt 时必须区分"还没读到"与"文件坏了"：
/// 前者要继续读下一个 chunk，后者必须立刻报错。把两者混成一个错误
/// 会让任何一次网络/文件分片读取都变成疑似损坏。
pub fn try_read_u64(input: &[u8]) -> Result<Option<(u64, usize)>> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (index, byte) in input.iter().enumerate() {
        if index >= 10 {
            return Err(Error::Trace("varint 超过 10 字节，文件可能已损坏".into()));
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(Some((value, index + 1)));
        }
        shift += 7;
    }
    Ok(None)
}

/// 读取 zigzag 编码的 `i64`。
pub fn read_zigzag(input: &[u8]) -> Result<(i64, usize)> {
    let (raw, used) = read_u64(input)?;
    Ok(((raw >> 1) as i64 ^ -((raw & 1) as i64), used))
}

/// 增量解码的 zigzag 版本。
pub fn try_read_zigzag(input: &[u8]) -> Result<Option<(i64, usize)>> {
    Ok(try_read_u64(input)?.map(|(raw, used)| ((raw >> 1) as i64 ^ -((raw & 1) as i64), used)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_u(value: u64) {
        let mut buf = Vec::new();
        write_u64(&mut buf, value);
        let (back, used) = read_u64(&buf).unwrap();
        assert_eq!(back, value);
        assert_eq!(used, buf.len());
    }

    fn roundtrip_i(value: i64) {
        let mut buf = Vec::new();
        write_zigzag(&mut buf, value);
        let (back, used) = read_zigzag(&buf).unwrap();
        assert_eq!(back, value);
        assert_eq!(used, buf.len());
    }

    #[test]
    fn varint_boundaries() {
        for v in [0u64, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            roundtrip_u(v);
        }
    }

    #[test]
    fn zigzag_boundaries() {
        for v in [
            0i64,
            -1,
            1,
            -64,
            63,
            -65,
            i32::MIN as i64,
            i64::MIN,
            i64::MAX,
        ] {
            roundtrip_i(v);
        }
    }

    #[test]
    fn small_negatives_cost_one_byte() {
        let mut buf = Vec::new();
        write_zigzag(&mut buf, -64);
        assert_eq!(buf.len(), 1);
        buf.clear();
        write_zigzag(&mut buf, 63);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn oversized_varint_is_rejected() {
        let bad = [0x80u8; 12];
        let err = read_u64(&bad).unwrap_err();
        assert!(matches!(err, Error::Trace(_)));
    }

    #[test]
    fn truncated_varint_is_rejected() {
        let bad = [0x80u8, 0x80];
        assert!(read_u64(&bad).is_err());
        assert!(read_u64(&[]).is_err());
    }

    #[test]
    fn incremental_decoder_reports_incomplete_separately() {
        assert_eq!(try_read_u64(&[]).unwrap(), None);
        assert_eq!(
            try_read_u64(&[0x80]).unwrap(),
            None,
            "续接位为 1 但后续字节缺失"
        );
        assert_eq!(try_read_u64(&[0x80, 0x01]).unwrap(), Some((128, 2)));
        assert!(
            try_read_u64(&[0x80; 12]).is_err(),
            "超长 varint 是损坏而不是未读完"
        );
        assert_eq!(try_read_zigzag(&[0x01]).unwrap(), Some((-1, 1)));
    }
}
