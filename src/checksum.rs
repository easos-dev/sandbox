//! 文件指纹与持有证明的纯计算：MD5、内容 ETag、两种 CRC64、HMAC-SHA256。
//!
//! 值的定义与 File 模块 `checksum.rs` 完全一致，否则助手拿着沙箱算出的指纹去 File 建上传会被判为内容不符：
//! - CRC-64/XZ（ECMA-182）：OSS 的 `x-oss-hash-crc64ecma`；CRC-64/NVME：S3/MinIO 的 `x-amz-checksum-crc64nvme`。两者都按十进制字符串给出。
//! - 内容 ETag：按 5 MiB 分片，一片时是整个文件的 MD5，多片时是各片 MD5 拼接后再 MD5 加 `-片数`。
//!
//! 全部是一遍流式计算：文件只读一次，每种值各维护自己的状态。

use hmac::Mac;
use md5::Digest;

/// CRC-64/XZ（ECMA-182）的反射多项式。
const ECMA: u64 = 0xC96C_5795_D787_0F42;
/// CRC-64/NVME 的反射多项式。
const NVME: u64 = 0x9A6C_9329_AC4B_C9B5;
/// 内容 ETag 的分片长度（字节）；必须与 File 模块的 PART_SIZE 相同，否则多片文件的 ETag 对不上。
pub const PART_SIZE: u64 = 5 * 1024 * 1024;

/// 一种 CRC64 的查表实现：初值与输出异或都是全 1，按字节反射计算。
pub struct Crc64 {
    table: [u64; 256],
    value: u64,
}

impl Crc64 {
    fn new(poly: u64) -> Self {
        let mut table = [0u64; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut crc = i as u64;
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ poly } else { crc >> 1 };
            }
            *slot = crc;
        }
        Self { table, value: !0 }
    }
    /// OSS 用的 CRC-64/XZ。
    pub fn ecma() -> Self {
        Self::new(ECMA)
    }
    /// S3/MinIO 用的 CRC-64/NVME。
    pub fn nvme() -> Self {
        Self::new(NVME)
    }
    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.value = self.table[((self.value ^ u64::from(*byte)) & 0xff) as usize] ^ (self.value >> 8);
        }
    }
    pub fn finish(&self) -> u64 {
        !self.value
    }
}

/// 小写十六进制。
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}


/// 一个分片的位置与校验值。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Part {
    /// 分片在文件里的起点（字节）。
    pub offset: u64,
    /// 分片长度（字节）；只有最后一片可能短于 PART_SIZE。
    pub length: u64,
    /// 分片内容的 MD5，小写十六进制（填 Content-MD5 头时由调用方转 base64）。
    pub md5: String,
    /// 分片内容的 CRC-64/NVME，十进制；S3/MinIO 分片上传时 File 把它签进这一片的请求头。
    pub crc64_nvme: String,
}

/// 一遍流式算出全部指纹。
pub struct Fingerprint {
    whole: md5::Md5,
    part: md5::Md5,
    /// 当前这一片的 CRC-64/NVME。
    part_nvme: Crc64,
    /// 当前这一片已经喂进去的字节数。
    part_filled: u64,
    /// 已完成的各片 MD5（二进制），算内容 ETag 用。
    part_digests: Vec<[u8; 16]>,
    parts: Vec<Part>,
    ecma: Crc64,
    nvme: Crc64,
    size: u64,
}

/// 指纹的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digests {
    /// 文件长度（字节）。
    pub size: u64,
    /// 整个文件的 MD5，小写十六进制。
    pub md5: String,
    /// 内容 ETag（File 的定义）。
    pub etag: String,
    /// CRC-64/XZ，十进制。
    pub crc64_ecma: String,
    /// CRC-64/NVME，十进制。
    pub crc64_nvme: String,
    /// 各分片（按 PART_SIZE），空文件也有一片（长度 0）。
    pub parts: Vec<Part>,
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Fingerprint {
    pub fn new() -> Self {
        Self {
            whole: md5::Md5::new(),
            part: md5::Md5::new(),
            part_nvme: Crc64::nvme(),
            part_filled: 0,
            part_digests: Vec::new(),
            parts: Vec::new(),
            ecma: Crc64::ecma(),
            nvme: Crc64::nvme(),
            size: 0,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.whole.update(bytes);
        self.ecma.update(bytes);
        self.nvme.update(bytes);
        self.size += bytes.len() as u64;
        // 分片 MD5 要按 PART_SIZE 的边界切，一个 chunk 可能跨片。
        while !bytes.is_empty() {
            let room = (PART_SIZE - self.part_filled) as usize;
            let take = room.min(bytes.len());
            self.part.update(&bytes[..take]);
            self.part_nvme.update(&bytes[..take]);
            self.part_filled += take as u64;
            bytes = &bytes[take..];
            if self.part_filled == PART_SIZE {
                self.close_part();
            }
        }
    }

    /// 结束当前这一片：记下它的 MD5 与 CRC-64/NVME，开始新的一片。
    fn close_part(&mut self) {
        let digest: [u8; 16] = std::mem::replace(&mut self.part, md5::Md5::new()).finalize().into();
        let nvme = std::mem::replace(&mut self.part_nvme, Crc64::nvme()).finish();
        let offset = self.size - self.part_filled;
        self.parts.push(Part {
            offset,
            length: self.part_filled,
            md5: hex(&digest),
            crc64_nvme: nvme.to_string(),
        });
        self.part_digests.push(digest);
        self.part_filled = 0;
    }

    pub fn finish(mut self) -> Digests {
        // 最后一片不满也要收；空文件也算一片，File 对 0 字节文件的 ETag 就是空串的 MD5。
        if self.part_filled > 0 || self.parts.is_empty() {
            self.close_part();
        }
        Digests {
            size: self.size,
            md5: hex(&self.whole.finalize()),
            etag: content_etag(&self.part_digests),
            crc64_ecma: self.ecma.finish().to_string(),
            crc64_nvme: self.nvme.finish().to_string(),
            parts: self.parts,
        }
    }
}

/// 内容 ETag：一片时是整个文件的 MD5；多片时是各片 MD5（二进制）拼接后再取 MD5，加 `-片数`。
pub fn content_etag(part_md5s: &[[u8; 16]]) -> String {
    match part_md5s {
        [single] => hex(single),
        parts => {
            let mut digest = md5::Md5::new();
            for part in parts {
                digest.update(part);
            }
            format!("{}-{}", hex(&digest.finalize()), parts.len())
        }
    }
}

/// 持有证明的一段：HMAC-SHA256(key, 段字节) 的标准 Base64（带填充，44 字符）。可分多次喂字节。
/// EasOS 规范：自己算出、只在接口里当字符串用的摘要一律标准 Base64，比十六进制短三成。
pub struct Proof {
    mac: hmac::Hmac<sha2::Sha256>,
}

impl Proof {
    pub fn new(key: &[u8]) -> Self {
        // HMAC 对任意长度的密钥都有定义，new_from_slice 不会失败。
        Self {
            mac: <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥"),
        }
    }
    pub fn update(&mut self, bytes: &[u8]) {
        self.mac.update(bytes);
    }
    pub fn finish(self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.mac.finalize().into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc64_与目录给出的校验值一致() {
        // CRC 目录（reveng）给出的 "123456789" 校验值：CRC-64/XZ 与 CRC-64/NVME；与 File 模块的测试相同。
        let mut ecma = Crc64::ecma();
        ecma.update(b"123456789");
        assert_eq!(ecma.finish(), 0x995D_C9BB_DF19_39FA);
        let mut nvme = Crc64::nvme();
        nvme.update(b"1234");
        nvme.update(b"56789");
        assert_eq!(nvme.finish(), 0xAE8B_1486_0A79_9888);
        assert_eq!(Crc64::ecma().finish(), 0);
    }

    #[test]
    fn 指纹一遍算出_md5_etag_与两种_crc64() {
        let mut fp = Fingerprint::new();
        fp.update(b"1234");
        fp.update(b"56789");
        let d = fp.finish();
        assert_eq!(d.size, 9);
        assert_eq!(d.md5, "25f9e794323b453885f5181f1b624d0b");
        assert_eq!(d.etag, d.md5, "单片时内容 ETag 就是整个文件的 MD5");
        assert_eq!(d.crc64_ecma, 0x995D_C9BB_DF19_39FAu64.to_string());
        assert_eq!(d.crc64_nvme, 0xAE8B_1486_0A79_9888u64.to_string());
        assert_eq!(d.parts, vec![Part { offset: 0, length: 9, md5: d.md5.clone(), crc64_nvme: d.crc64_nvme.clone() }]);

        let empty = Fingerprint::new().finish();
        assert_eq!(empty.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(empty.parts.len(), 1);
        assert_eq!(empty.crc64_ecma, "0");
    }

    #[test]
    fn 多片文件的分片按_5mib_边界切且跨_chunk_一致() {
        // 5 MiB + 3 字节：两片；分两种喂法结果必须相同。
        let data: Vec<u8> = (0..(PART_SIZE as usize + 3)).map(|i| (i % 251) as u8).collect();
        let mut one = Fingerprint::new();
        one.update(&data);
        let one = one.finish();
        let mut many = Fingerprint::new();
        for chunk in data.chunks(1_000_003) {
            many.update(chunk);
        }
        let many = many.finish();
        assert_eq!(one, many);
        assert_eq!(one.parts.len(), 2);
        assert_eq!(one.parts[0].length, PART_SIZE);
        let nvme_of = |bytes: &[u8]| {
            let mut crc = Crc64::nvme();
            crc.update(bytes);
            crc.finish().to_string()
        };
        assert_eq!(one.parts[0].crc64_nvme, nvme_of(&data[..PART_SIZE as usize]));
        assert_eq!(
            one.parts[1],
            Part {
                offset: PART_SIZE,
                length: 3,
                md5: hex(&md5::Md5::digest(&data[PART_SIZE as usize..])),
                crc64_nvme: nvme_of(&data[PART_SIZE as usize..]),
            }
        );
        assert!(one.etag.ends_with("-2"));
        // 内容 ETag 按定义重算一遍。
        let a: [u8; 16] = md5::Md5::digest(&data[..PART_SIZE as usize]).into();
        let b: [u8; 16] = md5::Md5::digest(&data[PART_SIZE as usize..]).into();
        assert_eq!(one.etag, content_etag(&[a, b]));
    }

    #[test]
    fn hmac_sha256_符合_rfc4231_测试向量() {
        // RFC 4231 Test Case 2：key = "Jefe"，data = "what do ya want for nothing?"。
        let mut proof = Proof::new(b"Jefe");
        proof.update(b"what do ya want ");
        proof.update(b"for nothing?");
        // 十六进制向量 5bdcc146…ec3843 的标准 Base64 写法。
        assert_eq!(proof.finish(), "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM=");
    }
}
