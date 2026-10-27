//! 内容哈希：与 TS `file-rewind.ts:588`
//! （`createHash("sha256").update(content).digest("hex")`）逐字节等价的实现。
//!
//! WHY 自己写而不是加依赖：native workspace 是纯本地构建（不联网拉 crate），而
//! `expectedHash` / `currentHash` 会进协议给客户端看，所以必须是**标准 SHA-256 十六进制**，
//! 不能用某个 host 私有的 hash——否则同一个文件在 TS host 与 Rust host 下给出不同字符串，
//! "外部已修改"的判定就成了一场巧合。
//! 常量表按定义生成（前 64 个质数立方根的小数部分前 32 位），正确性由
//! `matches_the_public_test_vectors` 的 NIST 向量钉住——包括跨块长度字段那条。

/// K：前 64 个质数立方根小数部分的前 32 位（FIPS 180-4 §4.2.2）。
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// H：前 8 个质数平方根小数部分的前 32 位（FIPS 180-4 §5.3.1）。
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// 小写十六进制的 SHA-256。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = H0;
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for block in message.chunks(64) {
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
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h = [
            h[0].wrapping_add(a),
            h[1].wrapping_add(b),
            h[2].wrapping_add(c),
            h[3].wrapping_add(d),
            h[4].wrapping_add(e),
            h[5].wrapping_add(f),
            h[6].wrapping_add(g),
            h[7].wrapping_add(hh),
        ];
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// 内容身份；文件不存在时的哨兵值与 TS 一致（`"missing"`）。
pub fn content_hash(content: Option<&str>) -> String {
    match content {
        Some(text) => sha256_hex(text.as_bytes()),
        None => "missing".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    /// NIST/公开向量：证明这不是"自造 hash"，而是标准 SHA-256。
    #[test]
    fn matches_the_public_test_vectors() {
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
        // 跨多个 64 字节块 + 长度字段进位到第二个块（1,000,000 个 'a'）。
        // 期望值与 Node `createHash("sha256")` 实测一致——那正是 TS host 用的实现。
        assert_eq!(
            sha256_hex(&[b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// 分块/padding 边界逐一对参考实现的摘要。
    ///
    /// 期望值来自独立参考实现（Python hashlib，与 Node `createHash("sha256")` 同源
    /// FIPS 180-4），覆盖 63/64/65（一个块内到跨块）、111/112/119/120/127/128
    /// （padding 后长度字段跨块的每一档）与 191/192。这些边界是自写 SHA-256 的
    /// 回归高发点：错一个位就会安静地给出"另一个也自洽"的摘要。
    #[test]
    fn block_and_padding_boundaries_match_a_reference_implementation() {
        const CASES: [(usize, &str); 11] = [
            (
                63,
                "85021e4907c21645631d596eee989bf43c2cd56b074175d5393804dbf1744412",
            ),
            (
                64,
                "72996563049cc84daa2c3f31fd5c3d10770e69d6ebbb8da5b6d76db303dbae43",
            ),
            (
                65,
                "57685f5e43ddac1567f4d404c357c44bab70744b8b6af6009760389170c5c062",
            ),
            (
                111,
                "386b0b03dcbf126115cc460cdc48e807d955c839b49ed0c6a55d38a111a16b23",
            ),
            (
                112,
                "9c5ec581e8e2e62a06423b74b38fee951a0390ce43142d9a42255ebec24feb44",
            ),
            (
                119,
                "de05957e72aca438f9b0c770cb2ee7fa0e58e08eb54536478db136d54c097abb",
            ),
            (
                120,
                "46a063da7dc0c878899dfc50adf2558256652c05dd1afc0b1341f7608c7a4776",
            ),
            (
                127,
                "87cdeb380917879fcd4c3b869ad644cbc7cbc6ff36b989cd4fe0f8123c13ca19",
            ),
            (
                128,
                "8169c725edd39f00140ffcf57c45b9c5143e2ffe375869e4cf2805342715d2b1",
            ),
            (
                191,
                "751f8191cd37a1c50db1f885426242b1c6f5792054cb78f81273c463765f2146",
            ),
            (
                192,
                "9e96fe6320d50c09026df65c9676a19e57fe86b26652cf513c2cc03015711df0",
            ),
        ];
        for (size, expected) in CASES {
            assert_eq!(
                sha256_hex(&vec![b'z'; size]),
                expected,
                "sha256 of {size} bytes of 'z' must match the reference"
            );
        }
    }

    #[test]
    fn utf8_inputs_are_hashed_by_bytes() {
        let text = "行内容 with mixed bytes 🚀".repeat(40);
        assert_eq!(sha256_hex(text.as_bytes()), sha256_hex(text.as_bytes()));
        assert_ne!(sha256_hex(text.as_bytes()), sha256_hex(b"other"));
    }
}
