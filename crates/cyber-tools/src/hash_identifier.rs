//! hash_identifier 工具：原生哈希识别与哈希计算工具。
//!
//! 具备两个核心功能：
//! 1. calculate: 原生计算常见安全哈希（MD5、SHA-1、SHA-256、SHA-512、NTLM、CRC32、HMAC）
//! 2. identify: 逆向匹配未知 Hash 字符串的特征，输出可能的哈希类型及 Hashcat / John the Ripper 模式编号

use std::fmt::Write as FmtWrite;
use std::future::Future;
use std::pin::Pin;

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use ring::digest::{Context, Digest, SHA1_FOR_LEGACY_USE_ONLY, SHA256, SHA512};
use ring::hmac;
use serde_json::{json, Value};

pub struct HashIdentifierTool;

impl Tool for HashIdentifierTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "hash_identifier".into(),
            description: "原生安全哈希计算与逆向识别工具。可快速计算 SHA1, SHA256, SHA512, MD5, NTLM, CRC32, HMAC，或输入未知密文自动推断 Hash 算法及 Hashcat mode 编号。".into(),
            tags: vec!["security".into(), "crypto".into(), "hash".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["calculate", "identify"],
                        "description": "操作类型：calculate (计算哈希), identify (识别未知哈希)"
                    },
                    "input": {
                        "type": "string",
                        "description": "待处理文本或哈希密文字符串"
                    },
                    "algorithm": {
                        "type": "string",
                        "enum": ["sha1", "sha256", "sha512", "md5", "ntlm", "crc32", "hmac_sha256"],
                        "description": "哈希算法（calculate 时必填）"
                    },
                    "key": {
                        "type": "string",
                        "description": "HMAC 模式下的对称密钥（可选）"
                    }
                },
                "required": ["action", "input"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let action = input
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("hash_identifier 缺少 action 参数".into()))?;

            let data = input
                .get("input")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("hash_identifier 缺少 input 参数".into()))?;

            match action {
                "identify" => {
                    let candidates = identify_hash(data);
                    let res = json!({
                        "input": data,
                        "length": data.trim().len(),
                        "possible_hashes": candidates
                    });
                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                "calculate" => {
                    let algo = input
                        .get("algorithm")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if algo.is_empty() {
                        return Ok(ToolOutput {
                            content: "calculate 操作必须指定 algorithm 参数 (sha1, sha256, sha512, md5, ntlm, crc32, hmac_sha256)".into(),
                            is_error: true,
                        });
                    }

                    let key = input.get("key").and_then(|v| v.as_str()).unwrap_or("");
                    match calculate_hash(data, algo, key) {
                        Ok(hash_val) => {
                            let res = json!({
                                "algorithm": algo,
                                "input_length": data.len(),
                                "result": hash_val
                            });
                            Ok(ToolOutput {
                                content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                                is_error: false,
                            })
                        }
                        Err(e) => Ok(ToolOutput {
                            content: format!("哈希计算失败: {e}"),
                            is_error: true,
                        }),
                    }
                }
                other => Ok(ToolOutput {
                    content: format!("不支持的操作: {other}"),
                    is_error: true,
                }),
            }
        })
    }
}

fn calculate_hash(data: &str, algo: &str, key: &str) -> std::result::Result<String, String> {
    match algo.to_lowercase().as_str() {
        "sha256" => {
            let mut ctx = Context::new(&SHA256);
            ctx.update(data.as_bytes());
            Ok(hex_digest(ctx.finish()))
        }
        "sha1" => {
            let mut ctx = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
            ctx.update(data.as_bytes());
            Ok(hex_digest(ctx.finish()))
        }
        "sha512" => {
            let mut ctx = Context::new(&SHA512);
            ctx.update(data.as_bytes());
            Ok(hex_digest(ctx.finish()))
        }
        "hmac_sha256" => {
            if key.is_empty() {
                return Err("HMAC 运算必须提供 key 参数".into());
            }
            let s_key = hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes());
            let tag = hmac::sign(&s_key, data.as_bytes());
            let mut s = String::new();
            for b in tag.as_ref() {
                write!(s, "{:02x}", b).unwrap();
            }
            Ok(s)
        }
        "crc32" => {
            let crc = crc32_compute(data.as_bytes());
            Ok(format!("{:08x}", crc))
        }
        "md5" => {
            // 轻量纯 Rust MD5 实现，免引入额外依赖
            let digest = md5_digest(data.as_bytes());
            let mut s = String::new();
            for b in digest {
                write!(s, "{:02x}", b).unwrap();
            }
            Ok(s)
        }
        "ntlm" => {
            // NTLM 哈希：Unicode (UTF-16LE) 编码后进行 MD4 计算
            let utf16_le: Vec<u8> = data.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let digest = md4_digest(&utf16_le);
            let mut s = String::new();
            for b in digest {
                write!(s, "{:02x}", b).unwrap();
            }
            Ok(s)
        }
        other => Err(format!("不支持的算法: {other}")),
    }
}

fn hex_digest(digest: Digest) -> String {
    let mut s = String::new();
    for b in digest.as_ref() {
        write!(s, "{:02x}", b).unwrap();
    }
    s
}

// 简明哈希类型结构
#[derive(serde::Serialize)]
struct HashCandidate {
    name: &'static str,
    hashcat_mode: Option<u32>,
    john_format: &'static str,
    confidence: &'static str,
}

fn identify_hash(data: &str) -> Vec<HashCandidate> {
    let trimmed = data.trim();
    let len = trimmed.len();
    let is_hex = trimmed.chars().all(|c| c.is_ascii_hexdigit());
    let mut candidates = Vec::new();

    if is_hex {
        match len {
            32 => {
                candidates.push(HashCandidate {
                    name: "MD5",
                    hashcat_mode: Some(0),
                    john_format: "raw-md5",
                    confidence: "High",
                });
                candidates.push(HashCandidate {
                    name: "NTLM",
                    hashcat_mode: Some(1000),
                    john_format: "nt",
                    confidence: "Medium",
                });
                candidates.push(HashCandidate {
                    name: "MD4",
                    hashcat_mode: Some(900),
                    john_format: "raw-md4",
                    confidence: "Low",
                });
            }
            40 => {
                candidates.push(HashCandidate {
                    name: "SHA-1",
                    hashcat_mode: Some(100),
                    john_format: "raw-sha1",
                    confidence: "High",
                });
                candidates.push(HashCandidate {
                    name: "MySQL 4.1+",
                    hashcat_mode: Some(300),
                    john_format: "mysql-sha1",
                    confidence: "Low",
                });
            }
            64 => {
                candidates.push(HashCandidate {
                    name: "SHA-256",
                    hashcat_mode: Some(1400),
                    john_format: "raw-sha256",
                    confidence: "High",
                });
                candidates.push(HashCandidate {
                    name: "HMAC-SHA256",
                    hashcat_mode: Some(1450),
                    john_format: "hmac-sha256",
                    confidence: "Medium",
                });
            }
            128 => {
                candidates.push(HashCandidate {
                    name: "SHA-512",
                    hashcat_mode: Some(1700),
                    john_format: "raw-sha512",
                    confidence: "High",
                });
            }
            8 => {
                candidates.push(HashCandidate {
                    name: "CRC32",
                    hashcat_mode: None,
                    john_format: "crc32",
                    confidence: "High",
                });
            }
            _ => {}
        }
    }

    if trimmed.starts_with("$2a$") || trimmed.starts_with("$2b$") || trimmed.starts_with("$2y$") {
        candidates.push(HashCandidate {
            name: "Bcrypt",
            hashcat_mode: Some(3200),
            john_format: "bcrypt",
            confidence: "Definite",
        });
    } else if trimmed.starts_with("$6$") {
        candidates.push(HashCandidate {
            name: "SHA512-Crypt",
            hashcat_mode: Some(1800),
            john_format: "sha512crypt",
            confidence: "Definite",
        });
    } else if trimmed.starts_with("$1$") {
        candidates.push(HashCandidate {
            name: "MD5-Crypt",
            hashcat_mode: Some(500),
            john_format: "md5crypt",
            confidence: "Definite",
        });
    }

    candidates
}

// 快速 CRC32
fn crc32_compute(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = -((crc & 1) as i32) as u32;
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// 快速 MD5
fn md5_digest(data: &[u8]) -> [u8; 16] {
    let mut msg = data.to_vec();
    let orig_len_bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&orig_len_bits.to_le_bytes());

    let mut a0 = 0x67452301u32;
    let mut b0 = 0xefcdab89u32;
    let mut c0 = 0x98badcfeu32;
    let mut d0 = 0x10325476u32;

    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    let k: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, b) in chunk.chunks(4).enumerate() {
            m[i] = u32::from_le_bytes(b.try_into().unwrap());
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };

            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                (a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g])).rotate_left(s[i]),
            );
            a = temp;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// 快速 MD4 (NTLM 计算核心)
fn md4_digest(data: &[u8]) -> [u8; 16] {
    let mut msg = data.to_vec();
    let orig_len_bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&orig_len_bits.to_le_bytes());

    let mut a0 = 0x67452301u32;
    let mut b0 = 0xefcdab89u32;
    let mut c0 = 0x98badcfeu32;
    let mut d0 = 0x10325476u32;

    for chunk in msg.chunks(64) {
        let mut x = [0u32; 16];
        for (i, b) in chunk.chunks(4).enumerate() {
            x[i] = u32::from_le_bytes(b.try_into().unwrap());
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        macro_rules! round1 {
            ($a:expr, $b:expr, $c:expr, $d:expr, $k:expr, $s:expr) => {
                $a = ($a.wrapping_add(($b & $c) | (!$b & $d)).wrapping_add(x[$k])).rotate_left($s);
            };
        }
        macro_rules! round2 {
            ($a:expr, $b:expr, $c:expr, $d:expr, $k:expr, $s:expr) => {
                $a = ($a
                    .wrapping_add(($b & $c) | ($b & $d) | ($c & $d))
                    .wrapping_add(x[$k])
                    .wrapping_add(0x5a827999))
                .rotate_left($s);
            };
        }
        macro_rules! round3 {
            ($a:expr, $b:expr, $c:expr, $d:expr, $k:expr, $s:expr) => {
                $a = ($a
                    .wrapping_add($b ^ $c ^ $d)
                    .wrapping_add(x[$k])
                    .wrapping_add(0x6ed9eba1))
                .rotate_left($s);
            };
        }

        // Round 1
        round1!(a, b, c, d, 0, 3);
        round1!(d, a, b, c, 1, 7);
        round1!(c, d, a, b, 2, 11);
        round1!(b, c, d, a, 3, 19);
        round1!(a, b, c, d, 4, 3);
        round1!(d, a, b, c, 5, 7);
        round1!(c, d, a, b, 6, 11);
        round1!(b, c, d, a, 7, 19);
        round1!(a, b, c, d, 8, 3);
        round1!(d, a, b, c, 9, 7);
        round1!(c, d, a, b, 10, 11);
        round1!(b, c, d, a, 11, 19);
        round1!(a, b, c, d, 12, 3);
        round1!(d, a, b, c, 13, 7);
        round1!(c, d, a, b, 14, 11);
        round1!(b, c, d, a, 15, 19);

        // Round 2
        round2!(a, b, c, d, 0, 3);
        round2!(d, a, b, c, 4, 5);
        round2!(c, d, a, b, 8, 9);
        round2!(b, c, d, a, 12, 13);
        round2!(a, b, c, d, 1, 3);
        round2!(d, a, b, c, 5, 5);
        round2!(c, d, a, b, 9, 9);
        round2!(b, c, d, a, 13, 13);
        round2!(a, b, c, d, 2, 3);
        round2!(d, a, b, c, 6, 5);
        round2!(c, d, a, b, 10, 9);
        round2!(b, c, d, a, 14, 13);
        round2!(a, b, c, d, 3, 3);
        round2!(d, a, b, c, 7, 5);
        round2!(c, d, a, b, 11, 9);
        round2!(b, c, d, a, 15, 13);

        // Round 3
        round3!(a, b, c, d, 0, 3);
        round3!(d, a, b, c, 8, 9);
        round3!(c, d, a, b, 4, 11);
        round3!(b, c, d, a, 12, 15);
        round3!(a, b, c, d, 2, 3);
        round3!(d, a, b, c, 10, 9);
        round3!(c, d, a, b, 6, 11);
        round3!(b, c, d, a, 14, 15);
        round3!(a, b, c, d, 1, 3);
        round3!(d, a, b, c, 9, 9);
        round3!(c, d, a, b, 5, 11);
        round3!(b, c, d, a, 13, 15);
        round3!(a, b, c, d, 3, 3);
        round3!(d, a, b, c, 11, 9);
        round3!(c, d, a, b, 7, 11);
        round3!(b, c, d, a, 15, 15);

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5_calculation() {
        let res = calculate_hash("admin", "md5", "").unwrap();
        assert_eq!(res, "21232f297a57a5a743894a0e4a801fc3");
    }

    #[test]
    fn test_sha256_calculation() {
        let res = calculate_hash("admin", "sha256", "").unwrap();
        assert_eq!(
            res,
            "8c6976e5b5410415bde908bd4dee15dfb167a9c873fc4bb8a81f6f2ab448a918"
        );
    }

    #[test]
    fn test_ntlm_calculation() {
        let res = calculate_hash("Password123", "ntlm", "").unwrap();
        assert_eq!(res, "58a478135a93ac3bf058a5ea0e8fdb71");
    }

    #[test]
    fn test_identify_md5() {
        let candidates = identify_hash("21232f297a57a5a743894a0e4a801fc3");
        assert!(candidates.iter().any(|c| c.name == "MD5"));
    }
}
