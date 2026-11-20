//! hash.rs — 字符串哈希工具。
//!
//! 重写自 `src/utils/hash.ts`。TS 端 Bun 环境用 `Bun.hash`（wyhash），
//! Node 环境用 SHA-256。Rust 端没有 Bun 运行时，所有路径统一为
//! Node 语义（SHA-256）；Cargo.toml 无 crypto/sha2 依赖，附一份
//! 纯 std 的 FIPS-180-4 SHA-256 实现。
//!
//! 已知差异：TS 的 Bun 分支返回 wyhash 的十进制字符串；本实现返回
//! SHA-256 hex（即 Node 路径的行为），跨平台稳定、适合落盘缓存键。

/// djb2 快速非加密哈希，返回有符号 32 位整数。
///
/// 确定性、跨运行时一致（不像 Bun.hash 用的 wyhash）。作为 Bun.hash
/// 不可用时的兜底，或需要落盘稳定输出（如缓存目录名必须挺过运行时
/// 升级）时使用。
///
/// 注意：与 JS 一样按 UTF-16 码元迭代（`charCodeAt`）。
pub fn djb2_hash(s: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in s.encode_utf16() {
        // hash = ((hash << 5) - hash + code) | 0 —— i32 环绕即 wrap
        hash = hash
            .wrapping_mul(32)
            .wrapping_sub(hash)
            .wrapping_add(unit as i32);
    }
    hash
}

/// 对任意内容做变更检测哈希。输出为 SHA-256 hex（Node 路径语义）。
pub fn hash_content(content: &str) -> String {
    sha256_hex(content.as_bytes())
}

/// 哈希两个字符串，不产生拼接临时串。
///
/// TS 的 Node 路径：SHA-256 用 a → '\0' → b 的增量更新；bun 路径用
/// 种子链。本实现统一为 Node 语义。
pub fn hash_pair(a: &str, b: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(a.as_bytes());
    hasher.update(b"\0");
    hasher.update(b.as_bytes());
    hasher.finalize_hex()
}

// --- 纯 std SHA-256（FIPS-180-4） ------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0u8; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        let mut data = data;
        if self.buffer_len > 0 {
            let take = (64 - self.buffer_len).min(data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize_hex(mut self) -> String {
        // 填充：0x80 然后 0 直到长度 ≡ 56 (mod 64)，末尾 8 字节大端位长
        let total_bits = self.total_len.wrapping_mul(8);
        let pad_len = 64 - ((self.buffer_len + 9) % 64).min(64);
        let mut padding = vec![0u8; pad_len + 9];
        padding[0] = 0x80;
        padding[pad_len + 1..pad_len + 9].copy_from_slice(&total_bits.to_be_bytes());
        self.update(&padding);

        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{:08x}", word));
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn djb2_known_values() {
        // 与 TS 实现共享同一确定性序列：空串为 0
        assert_eq!(djb2_hash(""), 0);
        // 手算："a" → 0*32 - 0 + 97 = 97
        assert_eq!(djb2_hash("a"), 97);
        // "ab" → 97*31 + 98 = 3105
        assert_eq!(djb2_hash("ab"), 3105);
        // 有符号回绕：长输入不 panic
        let _ = djb2_hash(&"x".repeat(10_000));
    }

    #[test]
    fn sha256_matches_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hash_content_and_pair() {
        assert_eq!(
            hash_content("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // hash_pair(a, b) 与 sha256(a + "\0" + b) 一致
        let mut joined = "ts".as_bytes().to_vec();
        joined.push(0);
        joined.extend_from_slice(b"code");
        assert_eq!(hash_pair("ts", "code"), sha256_hex(&joined));
        // 与 ("tsc", "ode") 不同 —— 分隔符消歧
        let mut other = "tsc".as_bytes().to_vec();
        other.push(0);
        other.extend_from_slice(b"ode");
        assert_ne!(hash_pair("ts", "code"), sha256_hex(&other));
    }

    #[test]
    fn long_input_across_block_boundaries() {
        assert_eq!(
            sha256_hex("a".repeat(1_000_000).as_bytes()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
