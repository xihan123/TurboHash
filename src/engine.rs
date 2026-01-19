// 自适应IO引擎模块

use crossbeam_channel::Sender;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Instant;

use crate::cache::CacheConfig;
use crate::error::{HashError, HashResult, IoErrorContext};
use crate::hash::FileHasher;

/// 进度更新消息
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub processed: u64,
    pub total: u64,
}

/// 系统信息
#[derive(Debug, Clone)]
pub struct SystemInfo {
    #[allow(dead_code)]
    pub available_memory: u64,
    #[allow(dead_code)]
    pub cpu_count: usize,
}

impl SystemInfo {
    pub fn detect() -> Self {
        #[cfg(target_os = "windows")]
        fn get_memory_info() -> (u64, u64) {
            use std::mem::MaybeUninit;
            use windows_sys::Win32::System::SystemInformation::{
                GlobalMemoryStatusEx, MEMORYSTATUSEX,
            };

            let mut stat: MaybeUninit<MEMORYSTATUSEX> = MaybeUninit::uninit();
            unsafe {
                (*stat.as_mut_ptr()).dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
                if GlobalMemoryStatusEx(stat.as_mut_ptr()) != 0 {
                    let stat = stat.assume_init();
                    (stat.ullTotalPhys, stat.ullAvailPhys)
                } else {
                    (8u64 * 1024 * 1024 * 1024, 4u64 * 1024 * 1024 * 1024)
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        fn get_memory_info() -> (u64, u64) {
            (8u64 * 1024 * 1024 * 1024, 4u64 * 1024 * 1024 * 1024)
        }

        let (_total_mem, avail_mem) = get_memory_info();
        let cpu_count = num_cpus::get_physical();

        SystemInfo {
            available_memory: avail_mem,
            cpu_count: cpu_count.max(1),
        }
    }

    pub fn recommend_buffer_sizes(&self) -> (usize, usize) {
        let buffer_size = ((self.available_memory / 1000) as usize).clamp(64 * 1024, 1024 * 1024);
        let mmap_chunk_size =
            ((self.available_memory / 100) as usize).clamp(1024 * 1024, 16 * 1024 * 1024);

        (buffer_size, mmap_chunk_size)
    }
}

pub fn detect_optimal_config() -> CacheConfig {
    let sys_info = SystemInfo::detect();
    let (buffer_size, mmap_chunk_size) = sys_info.recommend_buffer_sizes();

    CacheConfig {
        min_file_size: 1024 * 1024,
        retention_days: 30,
        buffer_size,
        mmap_chunk_size,
        auto_compute_enabled: CacheConfig::default().auto_compute_enabled,
        uppercase_display: CacheConfig::default().uppercase_display,
    }
}

const TINY_FILE_THRESHOLD: u64 = 64 * 1024;
const MEDIUM_FILE_THRESHOLD: u64 = 512 * 1024 * 1024;

fn format_hash_results(
    crc32: u32,
    md5: &[u8],
    sha1: &[u8],
    xxh3: &[u8],
) -> (String, String, String, String) {
    (
        format!("{:08x}", crc32),
        hex::encode(md5),
        hex::encode(sha1),
        hex::encode(xxh3),
    )
}

#[cfg(target_pointer_width = "32")]
fn check_chunk_size_fits(chunk_size: u64, path: &Path) -> HashResult<()> {
    if chunk_size > usize::MAX as u64 {
        return Err(HashError::SystemResource(format!(
            "Chunk size {} exceeds 32-bit system limit for file: {}",
            chunk_size,
            path.display()
        )));
    }
    Ok(())
}

pub fn compute_file_hash(
    path: &Path,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    buffer_size: usize,
    mmap_chunk_size: usize,
    file_size_hint: Option<u64>,
) -> HashResult<(String, String, String, String)> {
    let file_size = if let Some(size) = file_size_hint {
        size
    } else {
        std::fs::metadata(path).with_path(path)?.len()
    };

    let optimized_buffer_size = optimize_buffer_size(file_size, buffer_size);
    let optimized_chunk_size = optimize_chunk_size(file_size, mmap_chunk_size);

    if file_size < TINY_FILE_THRESHOLD {
        compute_hash_tiny(path, file_size)
    } else if file_size < MEDIUM_FILE_THRESHOLD {
        compute_hash_medium(path, file_size, progress_sender, optimized_buffer_size)
    } else {
        compute_hash_large(path, file_size, progress_sender, optimized_chunk_size)
    }
}

fn optimize_buffer_size(file_size: u64, default_buffer_size: usize) -> usize {
    let optimal_size = if file_size < 10 * 1024 * 1024 {
        (file_size / 4).max(64 * 1024).min(512 * 1024) as usize
    } else {
        (default_buffer_size * 2).min(2 * 1024 * 1024)
    };

    // 确保缓冲区大小是64KB的倍数（对齐优化）
    optimal_size.next_multiple_of(65536)
}

fn optimize_chunk_size(file_size: u64, default_chunk_size: usize) -> usize {
    let optimal_size = if file_size > 10u64 * 1024 * 1024 * 1024 {
        // 超大文件（>10GB）：使用更大块减少映射次数
        (default_chunk_size * 4).min(16 * 1024 * 1024)
    } else if file_size > 1024 * 1024 * 1024 {
        // 大文件（>1GB）：使用 8MB 块
        8 * 1024 * 1024
    } else {
        default_chunk_size
    };

    // 确保是大页对齐（2MB 对齐可提升性能）
    optimal_size.next_multiple_of(2 * 1024 * 1024)
}

fn compute_hash_tiny(path: &Path, _file_size: u64) -> HashResult<(String, String, String, String)> {
    let data = std::fs::read(path).with_path(path)?;

    let mut hasher = FileHasher::new();
    hasher.update(&data);
    let (crc32, md5, sha1, xxh3) = hasher.finalize().map_err(|e| {
        eprintln!("[Engine] 哈希计算失败: {}", e);
        e
    })?;

    Ok(format_hash_results(crc32, &md5, &sha1, &xxh3))
}

fn compute_hash_medium(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    buffer_size: usize,
) -> HashResult<(String, String, String, String)> {
    let file = File::open(path).with_path(path)?;
    let mut reader = BufReader::with_capacity(buffer_size, file);
    let mut hasher = FileHasher::new();

    let mut buffer = vec![0u8; buffer_size];
    let mut processed = 0u64;

    let progress_interval = (file_size / 100).max(1024 * 1024); // 至少1MB间隔
    let mut next_progress_threshold = progress_interval;

    loop {
        let n = reader.read(&mut buffer).with_path(path)?;
        if n == 0 {
            break;
        }

        hasher.update(&buffer[..n]);

        processed += n as u64;

        if let Some(sender) = progress_sender {
            if processed >= next_progress_threshold {
                let update = ProgressUpdate {
                    processed,
                    total: file_size,
                };
                let _ = sender.try_send(update);
                next_progress_threshold += progress_interval;
            }
        }
    }

    let (crc32, md5, sha1, xxh3) = hasher.finalize().map_err(|e| {
        eprintln!("[Engine] 哈希计算失败: {}", e);
        e
    })?;
    Ok(format_hash_results(crc32, &md5, &sha1, &xxh3))
}

fn compute_hash_large(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    mmap_chunk_size: usize,
) -> HashResult<(String, String, String, String)> {
    // 统一使用串行 mmap 处理，确保正确性
    // MD5/SHA1/CRC32 不支持并行状态合并，必须串行计算
    compute_hash_large_serial(path, file_size, progress_sender, mmap_chunk_size)
}

fn compute_hash_large_serial(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    mmap_chunk_size: usize,
) -> HashResult<(String, String, String, String)> {
    use memmap2::MmapOptions;

    let file = File::open(path).with_path(path)?;
    let file_len = file.metadata().with_path(path)?.len();

    #[cfg(target_pointer_width = "32")]
    check_chunk_size_fits(mmap_chunk_size as u64, path)?;

    let mut hasher = FileHasher::new();
    let mut processed = 0u64;

    let progress_interval = (file_size / 50).max(16 * 1024 * 1024); // 至少16MB间隔
    let mut next_progress_threshold = progress_interval;

    let mut offset = 0u64;
    while offset < file_len {
        let chunk_size = std::cmp::min(mmap_chunk_size as u64, file_len - offset) as usize;

        let mmap = unsafe {
            MmapOptions::new()
                .offset(offset)
                .len(chunk_size)
                .map(&file)
                .map_err(|e| HashError::Io(e, path.to_path_buf()))?
        };

        hasher.update(&mmap);
        processed += chunk_size as u64;
        offset += chunk_size as u64;

        if let Some(sender) = progress_sender {
            if processed >= next_progress_threshold {
                let update = ProgressUpdate {
                    processed,
                    total: file_size,
                };
                let _ = sender.try_send(update);
                next_progress_threshold += progress_interval;
            }
        }
    }

    let (crc32, md5, sha1, xxh3) = hasher.finalize().map_err(|e| {
        eprintln!("[Engine] 哈希计算失败: {}", e);
        e
    })?;
    Ok(format_hash_results(crc32, &md5, &sha1, &xxh3))
}

fn should_send_progress(last_update: &mut Instant, processed: u64, total: u64) -> bool {
    if processed == 0 {
        *last_update = Instant::now();
        return true;
    }

    let now = Instant::now();
    let time_elapsed = now.duration_since(*last_update).as_millis();
    let progress_delta = if total > 0 {
        (processed * 100 / total) - ((processed.saturating_sub(total / 100)) * 100 / total)
    } else {
        0
    };

    let should_update = time_elapsed > 100 || progress_delta > 1;
    if should_update {
        *last_update = now;
    }
    should_update
}

pub fn compute_xxhash3_only(
    path: &Path,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    buffer_size: usize,
    mmap_chunk_size: usize,
) -> HashResult<(String, u64)> {
    let file_size = std::fs::metadata(path).with_path(path)?.len();

    let xxhash3 = if file_size < TINY_FILE_THRESHOLD {
        compute_xxhash3_tiny(path)?
    } else if file_size < MEDIUM_FILE_THRESHOLD {
        compute_xxhash3_medium(path, file_size, progress_sender, buffer_size)?
    } else {
        compute_xxhash3_large(path, file_size, progress_sender, mmap_chunk_size)?
    };

    Ok((xxhash3, file_size))
}

pub fn compute_all_hashes_cached(
    path: &Path,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    buffer_size: usize,
    mmap_chunk_size: usize,
) -> HashResult<(String, String, String, String, u64)> {
    let file_size = std::fs::metadata(path).with_path(path)?.len();

    let (crc32, md5, sha1, xxhash3) = compute_file_hash(
        path,
        progress_sender,
        buffer_size,
        mmap_chunk_size,
        Some(file_size),
    )?;

    Ok((crc32, md5, sha1, xxhash3, file_size))
}

fn compute_xxhash3_tiny(path: &Path) -> HashResult<String> {
    use xxhash_rust::xxh3::Xxh3;

    let data = std::fs::read(path).with_path(path)?;
    let mut hasher = Xxh3::new();
    hasher.update(&data);
    let xxh3 = hasher.digest128();

    Ok(hex::encode(xxh3.to_be_bytes()))
}

fn compute_xxhash3_medium(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    buffer_size: usize,
) -> HashResult<String> {
    use xxhash_rust::xxh3::Xxh3;

    let file = File::open(path).with_path(path)?;
    let mut reader = BufReader::with_capacity(buffer_size, file);
    let mut hasher = Xxh3::new();

    let mut buffer = vec![0u8; buffer_size];
    let mut processed = 0u64;
    let mut last_update = Instant::now();

    loop {
        let n = reader.read(&mut buffer).with_path(path)?;
        if n == 0 {
            break;
        }

        hasher.update(&buffer[..n]);
        processed += n as u64;

        if let Some(sender) = progress_sender {
            if should_send_progress(&mut last_update, processed, file_size) {
                let update = ProgressUpdate {
                    processed,
                    total: file_size,
                };
                let _ = sender.try_send(update);
            }
        }
    }

    let xxh3 = hasher.digest128();
    Ok(hex::encode(xxh3.to_be_bytes()))
}

fn compute_xxhash3_large(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    mmap_chunk_size: usize,
) -> HashResult<String> {
    // 统一使用串行计算，确保正确性
    // xxhash-rust 不支持并行状态合并，必须使用原生流式 API
    compute_xxhash3_large_serial(path, file_size, progress_sender, mmap_chunk_size)
}

fn compute_xxhash3_large_serial(
    path: &Path,
    file_size: u64,
    progress_sender: Option<&Sender<ProgressUpdate>>,
    mmap_chunk_size: usize,
) -> HashResult<String> {
    use memmap2::MmapOptions;
    use xxhash_rust::xxh3::Xxh3;

    let file = File::open(path).with_path(path)?;
    let file_len = file.metadata().with_path(path)?.len();

    #[cfg(target_pointer_width = "32")]
    check_chunk_size_fits(mmap_chunk_size as u64, path)?;

    let mut hasher = Xxh3::new();
    let mut processed = 0u64;
    let mut last_update = Instant::now();

    let mut offset = 0u64;
    while offset < file_len {
        let chunk_size = std::cmp::min(mmap_chunk_size as u64, file_len - offset) as usize;

        let mmap = unsafe {
            MmapOptions::new()
                .offset(offset)
                .len(chunk_size)
                .map(&file)
                .map_err(|e| HashError::Io(e, path.to_path_buf()))?
        };

        hasher.update(&mmap);
        processed += chunk_size as u64;
        offset += chunk_size as u64;

        if let Some(sender) = progress_sender {
            if should_send_progress(&mut last_update, processed, file_size) {
                let update = ProgressUpdate {
                    processed,
                    total: file_size,
                };
                let _ = sender.try_send(update);
            }
        }
    }

    let xxh3 = hasher.digest128();
    Ok(hex::encode(xxh3.to_be_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_tiny_file() {
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file for test");
        temp_file
            .write_all(b"Hello, World!")
            .expect("Failed to write test data");

        let result = compute_file_hash(temp_file.path(), None, 64 * 1024, 1024 * 1024, None);
        assert!(
            result.is_ok(),
            "compute_file_hash failed: {:?}",
            result.err()
        );

        let (crc32, md5, sha1, xxh3) = result.unwrap();
        assert!(!crc32.is_empty());
        assert!(!md5.is_empty());
        assert!(!sha1.is_empty());
        assert!(!xxh3.is_empty());
    }

    #[test]
    fn test_should_send_progress() {
        let mut last_update = Instant::now();

        assert!(should_send_progress(&mut last_update, 0, 10000));

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!should_send_progress(&mut last_update, 50, 10000));

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(should_send_progress(&mut last_update, 100, 10000));
    }

    #[test]
    fn test_xxh3_correctness() {
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        // 创建 3MB 测试数据（使用串行计算）
        let test_data = vec![0xAB_u8; 3 * 1024 * 1024];
        temp_file
            .write_all(&test_data)
            .expect("Failed to write test data");
        temp_file.flush().expect("Failed to flush");

        let file_size = std::fs::metadata(temp_file.path()).unwrap().len();

        // 串行计算（所有文件统一使用串行，确保正确性）
        let serial_result =
            compute_xxhash3_large_serial(temp_file.path(), file_size, None, 1024 * 1024);

        // 验证结果有效
        assert!(serial_result.is_ok(), "xxHash3 computation failed");

        let serial_hash = serial_result.unwrap();

        // 验证哈希值不为空且格式正确（128位 = 32个十六进制字符）
        assert_eq!(serial_hash.len(), 32);
    }

    #[test]
    fn test_hash_consistency() {
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        // 创建 10MB 测试数据
        let test_data = vec![0x42_u8; 10 * 1024 * 1024];
        temp_file
            .write_all(&test_data)
            .expect("Failed to write test data");
        temp_file.flush().expect("Failed to flush");

        let file_size = std::fs::metadata(temp_file.path()).unwrap().len();

        // 使用不同的缓冲区大小计算哈希，结果应该一致
        let result1 = compute_file_hash(
            temp_file.path(),
            None,
            256 * 1024,
            4 * 1024 * 1024,
            Some(file_size),
        );

        let result2 = compute_file_hash(
            temp_file.path(),
            None,
            512 * 1024,
            8 * 1024 * 1024,
            Some(file_size),
        );

        assert!(result1.is_ok(), "First hash computation failed");
        assert!(result2.is_ok(), "Second hash computation failed");

        let (crc32_1, md5_1, sha1_1, xxh3_1) = result1.unwrap();
        let (crc32_2, md5_2, sha1_2, xxh3_2) = result2.unwrap();

        // 验证相同的文件产生相同的哈希值
        assert_eq!(crc32_1, crc32_2, "CRC32 mismatch");
        assert_eq!(md5_1, md5_2, "MD5 mismatch");
        assert_eq!(sha1_1, sha1_2, "SHA1 mismatch");
        assert_eq!(xxh3_1, xxh3_2, "xxHash3 mismatch");
    }

    #[test]
    fn test_medium_file_hash_correctness() {
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        // 创建 200MB 文件（中等大小）
        let chunk_size = 1024 * 1024;
        let chunks = 200;
        for i in 0..chunks {
            let data = vec![(i % 256) as u8; chunk_size];
            temp_file
                .write_all(&data)
                .expect("Failed to write test data");
        }
        temp_file.flush().expect("Failed to flush");

        let file_size = std::fs::metadata(temp_file.path()).unwrap().len();

        // 多次计算应该得到相同结果
        let result1 = compute_file_hash(
            temp_file.path(),
            None,
            256 * 1024,
            4 * 1024 * 1024,
            Some(file_size),
        );

        let result2 = compute_file_hash(
            temp_file.path(),
            None,
            256 * 1024,
            4 * 1024 * 1024,
            Some(file_size),
        );

        assert!(result1.is_ok(), "First hash computation failed");
        assert!(result2.is_ok(), "Second hash computation failed");

        let (crc32_1, md5_1, sha1_1, xxh3_1) = result1.unwrap();
        let (crc32_2, md5_2, sha1_2, xxh3_2) = result2.unwrap();

        assert_eq!(crc32_1, crc32_2, "CRC32 should be consistent");
        assert_eq!(md5_1, md5_2, "MD5 should be consistent");
        assert_eq!(sha1_1, sha1_2, "SHA1 should be consistent");
        assert_eq!(xxh3_1, xxh3_2, "xxHash3 should be consistent");
    }

    #[test]
    fn test_xxhash3_only_consistency() {
        let mut temp_file = NamedTempFile::new().expect("Failed to create temp file");
        let test_data = vec![0x55_u8; 5 * 1024 * 1024]; // 5MB
        temp_file
            .write_all(&test_data)
            .expect("Failed to write test data");
        temp_file.flush().expect("Failed to flush");

        // 多次计算 xxHash3 应该得到相同结果
        let result1 = compute_xxhash3_only(temp_file.path(), None, 256 * 1024, 4 * 1024 * 1024);
        let result2 = compute_xxhash3_only(temp_file.path(), None, 512 * 1024, 8 * 1024 * 1024);

        assert!(result1.is_ok(), "First xxHash3 computation failed");
        assert!(result2.is_ok(), "Second xxHash3 computation failed");

        let (xxh3_1, size1) = result1.unwrap();
        let (xxh3_2, size2) = result2.unwrap();

        assert_eq!(size1, size2, "File sizes should match");
        assert_eq!(xxh3_1, xxh3_2, "xxHash3 should be consistent");
    }
}
