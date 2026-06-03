//! 豆包服务端响应帧解析。
//!
//! 服务端可能下发三种 message type:
//! - `SERVER_FULL_RESPONSE`(0b1001):携带识别结果 JSON
//! - `SERVER_ERROR_RESPONSE`(0b1111):错误码 + 错误信息
//! - 其它 message type 也按容错方式处理
//!
//! 对应 sauc_go `response/response.go`。

use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::protocol::{
    gzip_decompress, COMPRESSION_GZIP, SERIALIZATION_JSON, SERVER_ERROR_RESPONSE,
    SERVER_FULL_RESPONSE,
};

/// 单句识别结果。当前 Handy 只读取 `result.text` 总文本,以下字段保留是为了将来支持
/// "分句进度展示"等功能时可零成本启用,因此用 `#[allow(dead_code)]` 屏蔽未读告警。
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AsrUtterance {
    #[serde(default)]
    pub definite: bool,
    #[serde(default)]
    pub start_time: i64,
    #[serde(default)]
    pub end_time: i64,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AsrResult {
    #[serde(default)]
    pub text: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub utterances: Vec<AsrUtterance>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AsrResponsePayload {
    #[serde(default)]
    pub result: AsrResult,
    /// 服务端在错误响应中可能放在这里(也可能放到外层错误码),容错保留。
    #[serde(default)]
    pub error: Option<String>,
}

/// 豆包二进制响应解析结果。
#[derive(Debug, Clone, Default)]
pub struct DoubaoResponse {
    /// 错误码:0 表示无错;非 0 (例如 45000001 / 45000151) 视为服务端错误。
    pub code: u32,
    /// 是否为最后一包(对应 message type specific flags 的 0b0010 位)。
    pub is_last_package: bool,
    /// 当前包的 sequence,服务端递增。
    pub sequence: i32,
    /// 解码后的业务 payload(无业务内容时为 `None`)。
    pub payload: Option<AsrResponsePayload>,
}

/// 解析豆包服务端二进制帧。
///
/// 失败原因(返回 `Err`):入参短于 4 字节 header / payload size 字段越界 / payload JSON 解析失败。
pub fn parse_response(msg: &[u8]) -> Result<DoubaoResponse> {
    if msg.len() < 4 {
        return Err(anyhow!("response too short: {} bytes", msg.len()));
    }

    let header_size = (msg[0] & 0x0f) as usize;
    let header_bytes = header_size * 4;
    if msg.len() < header_bytes {
        return Err(anyhow!(
            "response shorter than declared header ({} < {})",
            msg.len(),
            header_bytes
        ));
    }

    let message_type = msg[1] >> 4;
    let flags = msg[1] & 0x0f;
    let serialization = msg[2] >> 4;
    let compression = msg[2] & 0x0f;

    let mut cursor = header_bytes;
    let mut result = DoubaoResponse::default();

    // flags bit 0:携带 sequence
    if flags & 0x01 != 0 {
        if msg.len() < cursor + 4 {
            return Err(anyhow!("response missing sequence field"));
        }
        result.sequence = i32::from_be_bytes(msg[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
    }

    // flags bit 1:最后一包标志
    if flags & 0x02 != 0 {
        result.is_last_package = true;
    }

    // 按 message type 读取 size / code
    let payload_size = match message_type {
        SERVER_FULL_RESPONSE => {
            if msg.len() < cursor + 4 {
                return Err(anyhow!("full response missing payload size"));
            }
            let size = u32::from_be_bytes(msg[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            size
        }
        SERVER_ERROR_RESPONSE => {
            if msg.len() < cursor + 8 {
                return Err(anyhow!("error response missing code/size"));
            }
            result.code = u32::from_be_bytes(msg[cursor..cursor + 4].try_into().unwrap());
            cursor += 4;
            let size = u32::from_be_bytes(msg[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            size
        }
        _ => {
            // 其它 message type 不解析 payload(例如响应保留扩展类型)
            return Ok(result);
        }
    };

    if msg.len() < cursor + payload_size {
        return Err(anyhow!(
            "response payload truncated: need {} more, have {}",
            payload_size,
            msg.len() - cursor
        ));
    }

    let raw = &msg[cursor..cursor + payload_size];
    let decoded: Vec<u8> = if compression == COMPRESSION_GZIP {
        gzip_decompress(raw)?
    } else {
        raw.to_vec()
    };

    if decoded.is_empty() {
        return Ok(result);
    }

    if serialization == SERIALIZATION_JSON {
        let payload: AsrResponsePayload = serde_json::from_slice(&decoded).map_err(|e| {
            anyhow!(
                "invalid response JSON: {e}; raw={}",
                String::from_utf8_lossy(&decoded)
            )
        })?;
        result.payload = Some(payload);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_asr::doubao::protocol::{
        encode_header, gzip_compress, COMPRESSION_GZIP, NEG_SEQUENCE, POS_SEQUENCE,
        SERIALIZATION_JSON,
    };
    use byteorder::{BigEndian, WriteBytesExt};
    use std::io::Write;

    fn make_full_response(payload_json: &[u8], seq: i32, is_last: bool) -> Vec<u8> {
        let flags = if is_last {
            POS_SEQUENCE | NEG_SEQUENCE
        } else {
            POS_SEQUENCE
        };
        let header = encode_header(
            SERVER_FULL_RESPONSE,
            flags,
            SERIALIZATION_JSON,
            COMPRESSION_GZIP,
        );
        let compressed = gzip_compress(payload_json).unwrap();

        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        buf.write_i32::<BigEndian>(seq).unwrap();
        buf.write_u32::<BigEndian>(compressed.len() as u32).unwrap();
        buf.write_all(&compressed).unwrap();
        buf
    }

    #[test]
    fn test_parse_full_response_with_text() {
        let json = r#"{"result":{"text":"你好世界","utterances":[]}}"#.as_bytes();
        let frame = make_full_response(json, 42, false);

        let parsed = parse_response(&frame).unwrap();
        assert_eq!(parsed.code, 0);
        assert!(!parsed.is_last_package);
        assert_eq!(parsed.sequence, 42);
        assert_eq!(parsed.payload.unwrap().result.text, "你好世界");
    }

    #[test]
    fn test_parse_last_package_flag() {
        let json = br#"{"result":{"text":"final"}}"#;
        let frame = make_full_response(json, 7, true);
        let parsed = parse_response(&frame).unwrap();
        assert!(parsed.is_last_package);
        assert_eq!(parsed.payload.unwrap().result.text, "final");
    }

    #[test]
    fn test_parse_error_response() {
        // server error frame: header + code(4) + size(4) + payload
        let header = encode_header(
            SERVER_ERROR_RESPONSE,
            0,
            SERIALIZATION_JSON,
            COMPRESSION_GZIP,
        );
        let json = br#"{"error":"invalid params"}"#;
        let compressed = gzip_compress(json).unwrap();

        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        buf.write_u32::<BigEndian>(45_000_001).unwrap();
        buf.write_u32::<BigEndian>(compressed.len() as u32).unwrap();
        buf.write_all(&compressed).unwrap();

        let parsed = parse_response(&buf).unwrap();
        assert_eq!(parsed.code, 45_000_001);
        assert_eq!(parsed.payload.unwrap().error.unwrap(), "invalid params");
    }

    #[test]
    fn test_parse_short_input_returns_err() {
        assert!(parse_response(&[0x11, 0x91]).is_err());
    }
}
