//! 豆包 full client request / audio only request 的 payload 构造。
//!
//! 帧结构:`[4字节 header][4字节 seq(可选)][4字节 payload_size][payload]`
//! Payload 经 gzip 压缩,full client request 内层是 JSON,audio only 内层是 PCM 字节流。
//!
//! 参考 `docs/sauc_go/request/payload.go`。

use anyhow::Result;
use byteorder::{BigEndian, WriteBytesExt};
use serde::Serialize;
use std::io::Write;

use super::protocol::{
    encode_header, gzip_compress, CLIENT_AUDIO_ONLY_REQUEST, CLIENT_FULL_REQUEST, COMPRESSION_GZIP,
    NEG_WITH_SEQUENCE, POS_SEQUENCE, SERIALIZATION_JSON, SERIALIZATION_NONE,
};

/// 用户元信息(用于服务端日志过滤,可全空)。
#[derive(Debug, Clone, Serialize, Default)]
pub struct UserMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

/// 音频元信息。Handy 固定送 16 kHz / 16 bit / 单声道 / 原始 PCM。
#[derive(Debug, Clone, Serialize)]
pub struct AudioMeta {
    pub format: &'static str,
    pub codec: &'static str,
    pub rate: u32,
    pub bits: u32,
    pub channel: u32,
    /// BCP-47 语言代码,如 `"zh-CN"` / `"en-US"`。`None` 时由服务端按默认中英文+方言识别。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// 请求级开关。当前固定开 itn / punc,关 ddc / show_utterances 以减小响应体积。
#[derive(Debug, Clone, Serialize)]
pub struct RequestMeta {
    pub model_name: &'static str,
    pub enable_itn: bool,
    pub enable_punc: bool,
    pub enable_ddc: bool,
    pub show_utterances: bool,
}

/// Full client request 的 JSON 顶层结构。
#[derive(Debug, Clone, Serialize)]
pub struct AsrRequestPayload {
    pub user: UserMeta,
    pub audio: AudioMeta,
    pub request: RequestMeta,
}

impl AsrRequestPayload {
    /// 默认 Handy 配置:开 itn + 标点,关分句细节(只取最终 text)。
    pub fn handy_default(language: Option<String>) -> Self {
        Self {
            user: UserMeta {
                uid: Some("handy".to_string()),
            },
            audio: AudioMeta {
                format: "pcm",
                codec: "raw",
                rate: 16000,
                bits: 16,
                channel: 1,
                language,
            },
            request: RequestMeta {
                model_name: "bigmodel",
                enable_itn: true,
                enable_punc: true,
                enable_ddc: false,
                show_utterances: false,
            },
        }
    }
}

/// 构造 full client request 字节流(首包)。
///
/// 帧布局:
/// ```text
/// [header 4B][seq=1 i32 BE 4B][payload_size i32 BE 4B][gzip(JSON) Nb]
/// ```
///
/// `language` 透传给 `audio.language`。注意:**只有 bigmodel_nostream 接口支持 language 字段**,
/// 双向流式(含优化版 bigmodel_async)不支持。转录已切到 async 优化版,调用方应传 `None`;
/// `language` 形参仅为将来若回退 nostream 时保留。
pub fn build_full_client_request(language: Option<String>) -> Result<Vec<u8>> {
    let header = encode_header(
        CLIENT_FULL_REQUEST,
        POS_SEQUENCE,
        SERIALIZATION_JSON,
        COMPRESSION_GZIP,
    );
    let payload = AsrRequestPayload::handy_default(language);
    let json = serde_json::to_vec(&payload)?;
    let compressed = gzip_compress(&json)?;

    let mut buf = Vec::with_capacity(header.len() + 4 + 4 + compressed.len());
    buf.extend_from_slice(&header);
    buf.write_i32::<BigEndian>(1)?; // seq=1 首包
    buf.write_u32::<BigEndian>(compressed.len() as u32)?;
    buf.write_all(&compressed)?;
    Ok(buf)
}

/// 构造 audio only request 字节流。
///
/// `seq` 为正:中间包(flags = `POS_SEQUENCE`);
/// `seq` 为负:末包(flags = `NEG_WITH_SEQUENCE`),原始绝对值即正常序号,负号本身是末包标志。
pub fn build_audio_only_request(seq: i32, segment: &[u8]) -> Result<Vec<u8>> {
    let flags = if seq < 0 {
        NEG_WITH_SEQUENCE
    } else {
        POS_SEQUENCE
    };
    let header = encode_header(
        CLIENT_AUDIO_ONLY_REQUEST,
        flags,
        SERIALIZATION_NONE,
        COMPRESSION_GZIP,
    );
    let compressed = gzip_compress(segment)?;

    let mut buf = Vec::with_capacity(header.len() + 4 + 4 + compressed.len());
    buf.extend_from_slice(&header);
    buf.write_i32::<BigEndian>(seq)?;
    buf.write_u32::<BigEndian>(compressed.len() as u32)?;
    buf.write_all(&compressed)?;
    Ok(buf)
}

/// 把 16 kHz 单声道 f32 样本转换为豆包要求的 `pcm_s16le` 字节流。
///
/// 输入范围 `[-1.0, 1.0]`,超出会被 clamp。
pub fn f32_samples_to_pcm_s16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        out.extend_from_slice(&i16_sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::ReadBytesExt;
    use std::io::Cursor;

    use crate::cloud_asr::doubao::protocol::gzip_decompress;

    #[test]
    fn test_full_client_request_layout_and_json() {
        let raw = build_full_client_request(Some("zh-CN".to_string())).unwrap();

        // header
        assert_eq!(raw[0], 0x11);
        assert_eq!(raw[1], 0x11); // CLIENT_FULL_REQUEST << 4 | POS_SEQUENCE
        assert_eq!(raw[2], 0x11); // SERIALIZATION_JSON << 4 | COMPRESSION_GZIP
        assert_eq!(raw[3], 0x00);

        // seq + payload_size
        let mut cursor = Cursor::new(&raw[4..]);
        let seq = cursor.read_i32::<BigEndian>().unwrap();
        let size = cursor.read_u32::<BigEndian>().unwrap() as usize;
        assert_eq!(seq, 1);

        let payload_start = 12;
        let compressed = &raw[payload_start..payload_start + size];
        let json_bytes = gzip_decompress(compressed).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();

        assert_eq!(parsed["audio"]["rate"], 16000);
        assert_eq!(parsed["audio"]["bits"], 16);
        assert_eq!(parsed["audio"]["channel"], 1);
        assert_eq!(parsed["audio"]["format"], "pcm");
        assert_eq!(parsed["audio"]["language"], "zh-CN");
        assert_eq!(parsed["request"]["model_name"], "bigmodel");
        assert_eq!(parsed["request"]["enable_itn"], true);
    }

    #[test]
    fn test_full_client_request_omits_language_when_none() {
        let raw = build_full_client_request(None).unwrap();
        let payload_start = 12;
        let size = u32::from_be_bytes(raw[8..12].try_into().unwrap()) as usize;
        let compressed = &raw[payload_start..payload_start + size];
        let parsed: serde_json::Value =
            serde_json::from_slice(&gzip_decompress(compressed).unwrap()).unwrap();
        assert!(parsed["audio"].get("language").is_none());
    }

    #[test]
    fn test_audio_only_request_neg_seq_uses_neg_flags() {
        let raw = build_audio_only_request(-5, b"raw audio bytes").unwrap();
        // header[1] = (CLIENT_AUDIO_ONLY_REQUEST << 4) | NEG_WITH_SEQUENCE = 0x23
        assert_eq!(raw[1], 0x23);
        // seq i32 BE
        let seq = i32::from_be_bytes(raw[4..8].try_into().unwrap());
        assert_eq!(seq, -5);
    }

    #[test]
    fn test_audio_only_request_pos_seq_uses_pos_flags() {
        let raw = build_audio_only_request(7, b"raw audio bytes").unwrap();
        assert_eq!(raw[1], 0x21); // CLIENT_AUDIO_ONLY_REQUEST << 4 | POS_SEQUENCE
        let seq = i32::from_be_bytes(raw[4..8].try_into().unwrap());
        assert_eq!(seq, 7);
    }

    #[test]
    fn test_pcm_conversion_boundary_values() {
        let samples = [-1.0f32, 0.0, 1.0, 0.5, -0.5, 2.0, -2.0];
        let bytes = f32_samples_to_pcm_s16le(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);

        let read_i16 = |off: usize| i16::from_le_bytes([bytes[off], bytes[off + 1]]);
        assert_eq!(read_i16(0), -32767);
        assert_eq!(read_i16(2), 0);
        assert_eq!(read_i16(4), 32767);
        assert_eq!(read_i16(6), 16383); // 0.5 * 32767 = 16383.5 -> trunc 16383
        assert_eq!(read_i16(8), -16383);
        // clamp
        assert_eq!(read_i16(10), 32767);
        assert_eq!(read_i16(12), -32767);
    }
}
