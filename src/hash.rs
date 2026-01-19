use crate::error::{HashError, HashResult};
use crc32fast::Hasher as Crc32Hasher;
use md5::{Digest, Md5};
use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use xxhash_rust::xxh3::Xxh3;

pub struct FileHasher {
    crc32_hasher: Crc32Hasher,
    md5_hasher: Md5,
    sha1_context: Context,
    xxh3_hasher: Xxh3,
}

impl FileHasher {
    pub fn new() -> Self {
        Self {
            crc32_hasher: Crc32Hasher::new(),
            md5_hasher: Md5::new(),
            sha1_context: Context::new(&SHA1_FOR_LEGACY_USE_ONLY),
            xxh3_hasher: Xxh3::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.crc32_hasher.update(data);
        self.md5_hasher.update(data);
        self.sha1_context.update(data);
        self.xxh3_hasher.update(data);
    }

    pub fn finalize(self) -> HashResult<(u32, [u8; 16], [u8; 20], [u8; 16])> {
        let crc32 = self.crc32_hasher.finalize();

        let md5_digest = self.md5_hasher.finalize();
        let md5: [u8; 16] = md5_digest.as_slice().try_into().map_err(|_| {
            HashError::SystemResource("MD5 哈希输出大小不匹配: 预期 16 字节".to_string())
        })?;

        let sha1_digest = self.sha1_context.finish();
        let sha1: [u8; 20] = sha1_digest.as_ref().try_into().map_err(|_| {
            HashError::SystemResource("SHA1 哈希输出大小不匹配: 预期 20 字节".to_string())
        })?;

        let xxh3_arr: [u8; 16] = self.xxh3_hasher.digest128().to_be_bytes();

        Ok((crc32, md5, sha1, xxh3_arr))
    }
}

impl Default for FileHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_calculation() {
        let mut hasher = FileHasher::new();
        hasher.update(b"Hello, World!");

        let result = hasher.finalize();
        assert!(result.is_ok(), "finalize should succeed");

        let (crc32, md5, sha1, xxh3) = result.unwrap();

        assert_ne!(crc32, 0);
        assert_ne!(md5, [0u8; 16]);
        assert_ne!(sha1, [0u8; 20]);
        assert_ne!(xxh3, [0u8; 16]);
    }

    #[test]
    fn test_empty_hash() {
        let hasher = FileHasher::new();
        let result = hasher.finalize();
        assert!(result.is_ok(), "finalize should succeed");

        let (crc32, md5, sha1, xxh3) = result.unwrap();

        assert_eq!(crc32, 0);
        assert_eq!(hex::encode(md5), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            hex::encode(sha1),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(hex::encode(xxh3), "99aa06d3014798d86001c324468d497f");
    }
}
