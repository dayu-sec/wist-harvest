//! 长度前缀帧。
//!
//! **数据面的 `framing = "len"` 不是二进制前缀**，而是「ASCII 十进制长度 + 一个空格 + 正文」：
//!
//! ```text
//! 1234 {"schema":...} DSNAP: {...}
//!       ^ 一个空格分隔，长度只算后面的正文
//! ```
//!
//! 依据是 `wist-agentd/src/telemetry/warp_parse.rs` 的 `build_payload_bytes`
//! （`TcpFraming::Len`）：`data.len().to_string()` + 一个空格 + `data`。
//!
//! 之前按「u32 大端」实现，实测对 warp-parse 完全不可见：引擎把 `\x00` 当成长度文本，
//! 一直等后续字节，`pick_stat` 恒为 0，连接最终以 `peer closed during wait` 结束 ——
//! 帧看起来"发出去了"，数据面一条也没收到。
//!
//! 一帧 = 一份完整快照，不做分片（拆 `snap_id` 会让下游的清扫作用域失真）。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 长度前缀允许的最长十进制位数（防御对端报一个天文数字）。
pub const MAX_LEN_DIGITS: usize = 12;

#[derive(Debug)]
pub struct FrameError {
    pub message: String,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FrameError {}

impl From<std::io::Error> for FrameError {
    fn from(err: std::io::Error) -> Self {
        Self {
            message: format!("帧 IO 错误: {err}"),
        }
    }
}

/// 编码一帧：`<十进制长度> <正文>`。
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_LEN_DIGITS + 1 + payload.len());
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// 读一帧。`max_len` 是防御上限，防止对端报一个巨大的长度把内存打爆。
pub async fn read_frame<W>(stream: &mut W, max_len: usize) -> Result<Vec<u8>, FrameError>
where
    W: AsyncRead + Unpin,
{
    let mut digits = Vec::with_capacity(MAX_LEN_DIGITS);
    loop {
        let byte = stream.read_u8().await?;
        if byte == b' ' {
            break;
        }
        if !byte.is_ascii_digit() {
            return Err(FrameError {
                message: format!("帧头不是十进制长度（遇到字节 0x{byte:02x}）"),
            });
        }
        digits.push(byte);
        if digits.len() > MAX_LEN_DIGITS {
            return Err(FrameError {
                message: format!("帧头超过 {MAX_LEN_DIGITS} 位，拒绝"),
            });
        }
    }
    if digits.is_empty() {
        return Err(FrameError {
            message: "收到空长度帧头".to_string(),
        });
    }
    let len: usize = std::str::from_utf8(&digits)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| FrameError {
            message: "帧头长度解析失败".to_string(),
        })?;
    if len == 0 {
        return Err(FrameError {
            message: "收到零长度帧".to_string(),
        });
    }
    if len > max_len {
        return Err(FrameError {
            message: format!("帧长度 {len} 超过上限 {max_len}"),
        });
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// 写一帧并 flush。
pub async fn write_frame<W>(stream: &mut W, payload: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    stream.write_all(&encode(payload)).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_ascii_decimal_then_space() {
        let frame = encode(b"{\"a\":1}");
        assert_eq!(&frame[..8], b"7 {\"a\":1");
    }

    #[tokio::test]
    async fn roundtrip() {
        let payload = br#"{"status":"ok"}"#.to_vec();
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &payload).await.unwrap();
        let mut cursor = buffer.as_slice();
        let read = read_frame(&mut cursor, 1024).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let buffer = encode(&[0u8; 10]);
        let mut cursor = buffer.as_slice();
        let err = read_frame(&mut cursor, 4).await.unwrap_err();
        assert!(err.message.contains("超过上限"), "{}", err.message);
    }

    #[tokio::test]
    async fn rejects_non_digit_header() {
        let mut cursor = &b"\x00\x00\x00\x07abc"[..];
        let err = read_frame(&mut cursor, 1024).await.unwrap_err();
        assert!(err.message.contains("不是十进制长度"), "{}", err.message);
    }
}
