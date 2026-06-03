//! 豆包 ASR 自定义二进制协议常量与 header 编码。
//!
//! 协议规格见 `docs/豆包语音输入接入.md` "WebSocket 二进制协议" 一节。
//! 每帧 4 字节 header 的 bit 结构:
//!
//! ```text
//! Byte 0:  ProtocolVersion (4) | HeaderSize (4)         // 一律 0b0001 / 0b0001
//! Byte 1:  MessageType (4)     | MessageTypeFlags (4)
//! Byte 2:  Serialization (4)   | Compression (4)
//! Byte 3:  Reserved (8)                                  // 0x00
//! ```

use std::io::{Read, Write};

use anyhow::{anyhow, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};

pub const PROTOCOL_VERSION: u8 = 0b0001;
pub const HEADER_SIZE_VALUE: u8 = 0b0001; // 实际 header 字节数 = HEADER_SIZE_VALUE * 4

// Message type
pub const CLIENT_FULL_REQUEST: u8 = 0b0001;
pub const CLIENT_AUDIO_ONLY_REQUEST: u8 = 0b0010;
pub const SERVER_FULL_RESPONSE: u8 = 0b1001;
pub const SERVER_ERROR_RESPONSE: u8 = 0b1111;

// Message type specific flags(`NO_SEQUENCE` / `NEG_SEQUENCE` 暂未直接使用,但保留以照搬协议文档定义)。
#[allow(dead_code)]
pub const NO_SEQUENCE: u8 = 0b0000;
pub const POS_SEQUENCE: u8 = 0b0001;
#[allow(dead_code)]
pub const NEG_SEQUENCE: u8 = 0b0010;
pub const NEG_WITH_SEQUENCE: u8 = 0b0011;

// Serialization
pub const SERIALIZATION_NONE: u8 = 0b0000;
pub const SERIALIZATION_JSON: u8 = 0b0001;

// Compression
/// 未使用,保留供将来扩展;`encode_header` 的入参就允许任意压缩字段值。
#[allow(dead_code)]
pub const COMPRESSION_NONE: u8 = 0b0000;
pub const COMPRESSION_GZIP: u8 = 0b0001;

/// 编码 4 字节协议 header。
///
/// 各字段含义参考模块顶部说明。该函数对入参不做范围校验(调用方使用模块内常量即可保证合法)。
pub fn encode_header(message_type: u8, flags: u8, serialization: u8, compression: u8) -> [u8; 4] {
    [
        (PROTOCOL_VERSION << 4) | HEADER_SIZE_VALUE,
        (message_type << 4) | (flags & 0x0f),
        (serialization << 4) | (compression & 0x0f),
        0x00,
    ]
}

/// 用 gzip 压缩字节序列(默认压缩级别)。
pub fn gzip_compress(input: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(input)
        .map_err(|e| anyhow!("gzip write failed: {e}"))?;
    encoder
        .finish()
        .map_err(|e| anyhow!("gzip finish failed: {e}"))
}

/// 解压 gzip 字节序列。
pub fn gzip_decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| anyhow!("gzip decompress failed: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full client request 的标准 header:0x11 0x11 0x11 0x00。
    /// 与 sauc_go `request/header.go` 默认值对照。
    #[test]
    fn test_encode_header_full_client_request() {
        let header = encode_header(
            CLIENT_FULL_REQUEST,
            POS_SEQUENCE,
            SERIALIZATION_JSON,
            COMPRESSION_GZIP,
        );
        assert_eq!(header, [0x11, 0x11, 0x11, 0x00]);
    }

    /// 末包音频请求(audio only + 负 seq):header[1] 低 4 位 = NEG_WITH_SEQUENCE。
    #[test]
    fn test_encode_header_audio_only_with_neg_seq() {
        let header = encode_header(
            CLIENT_AUDIO_ONLY_REQUEST,
            NEG_WITH_SEQUENCE,
            SERIALIZATION_NONE,
            COMPRESSION_GZIP,
        );
        // byte0 = 0x11, byte1 = (0010<<4)|0011 = 0x23, byte2 = (0000<<4)|0001 = 0x01
        assert_eq!(header, [0x11, 0x23, 0x01, 0x00]);
    }

    #[test]
    fn test_gzip_round_trip() {
        let input = b"the quick brown fox jumps over the lazy dog";
        let compressed = gzip_compress(input).unwrap();
        assert!(!compressed.is_empty());
        let decompressed = gzip_decompress(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_gzip_empty_input_round_trip() {
        let compressed = gzip_compress(&[]).unwrap();
        let decompressed = gzip_decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }
}
