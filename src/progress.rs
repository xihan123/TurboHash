//! 全局进度跟踪器 - 实时累加处理中文件的进度
//!
//! 此模块提供实时全局进度计算，消除大文件处理期间的90%+停滞问题。

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// 文件进度状态
#[derive(Debug, Clone)]
pub struct FileProgress {
    /// 已处理的字节数
    pub processed: u64,
    /// 文件总字节数
    pub total: u64,
}

/// 全局进度跟踪器
///
/// # 核心功能
///
/// 1. **实时进度累加**：将处理中文件的已处理字节计入全局进度
/// 2. **线程安全**：使用AtomicU64和RwLock确保并发安全
/// 3. **自动清理**：文件完成时自动从处理中列表移除
///
/// # 使用示例
///
/// ```rust
/// let tracker = ProgressTracker::new();
/// tracker.set_total(1000);
///
/// // 开始处理文件
/// tracker.start_file(path1.clone(), 500);
/// tracker.start_file(path2.clone(), 500);
///
/// // 更新进度
/// tracker.update_progress(&path1, 250);
/// assert_eq!(tracker.get_global_progress(), 0.25); // 250/1000
///
/// // 完成文件
/// tracker.complete_file(&path1);
/// assert_eq!(tracker.get_global_progress(), 0.5); // 500/1000
/// ```
pub struct ProgressTracker {
    // 完成的字节数（原子操作，无需锁）
    processed_bytes: Arc<AtomicU64>,
    // 总字节数（原子操作，无需锁）
    total_bytes: Arc<AtomicU64>,
    // 处理中的文件进度（读写锁，支持高并发读）
    in_progress: Arc<RwLock<HashMap<PathBuf, FileProgress>>>,
}

impl ProgressTracker {
    /// 创建新的进度跟踪器
    pub fn new() -> Self {
        Self {
            processed_bytes: Arc::new(AtomicU64::new(0)),
            total_bytes: Arc::new(AtomicU64::new(0)),
            in_progress: Arc::new(RwLock::new(HashMap::new())),
        }
    }


    pub fn set_total(&self, total: u64) {
        self.total_bytes.store(total, Ordering::Relaxed);
    }

    pub fn start_file(&self, path: PathBuf, total: u64) {
        if let Ok(mut guard) = self.in_progress.write() {
            guard.insert(path, FileProgress {
                processed: 0,
                total,
            });
        }
        // 如果锁被毒化，忽略错误（此时应用程序可能已经处于不可恢复状态）
    }

    pub fn update_progress(&self, path: &Path, processed: u64) {
        if let Ok(mut guard) = self.in_progress.write() {
            if let Some(progress) = guard.get_mut(path) {
                progress.processed = processed;
            }
        }
    }


    pub fn complete_file(&self, path: &Path) {
        if let Ok(mut guard) = self.in_progress.write() {
            if let Some(progress) = guard.remove(path) {
                // 将文件的总字节数计入已完成字节
                self.processed_bytes.fetch_add(progress.total, Ordering::Relaxed);
            }
        }
    }

    pub fn get_global_progress(&self) -> f64 {
        let total = self.total_bytes.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }

        let processed = self.processed_bytes.load(Ordering::Relaxed);

        // 加上处理中文件的已处理字节
        let in_progress_bytes: u64 = self
            .in_progress
            .read()
            .map(|guard| guard.values().map(|p| p.processed).sum())
            .unwrap_or(0);

        let total_processed = processed + in_progress_bytes;
        total_processed as f64 / total as f64
    }

    pub fn reset(&self) {
        self.processed_bytes.store(0, Ordering::Relaxed);
        self.total_bytes.store(0, Ordering::Relaxed);
        if let Ok(mut guard) = self.in_progress.write() {
            guard.clear();
        }
    }

    /// 获取已完成字节数（主要用于测试）
    #[cfg(test)]
    pub fn get_processed_bytes(&self) -> u64 {
        self.processed_bytes.load(Ordering::Relaxed)
    }

    /// 获取总字节数（主要用于测试）
    #[cfg(test)]
    pub fn get_total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// 获取处理中的文件数量（主要用于测试）
    #[cfg(test)]
    pub fn get_in_progress_count(&self) -> usize {
        self.in_progress.read().map(|guard| guard.len()).unwrap_or(0)
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_tracker() {
        let tracker = ProgressTracker::new();
        let path1 = PathBuf::from("/test/file1.txt");
        let path2 = PathBuf::from("/test/file2.txt");

        // 设置总字节数
        tracker.set_total(1000);

        // 初始进度为0
        assert_eq!(tracker.get_global_progress(), 0.0);

        // 开始处理文件1
        tracker.start_file(path1.clone(), 500);
        tracker.start_file(path2.clone(), 500);

        // 更新文件1进度到50%
        tracker.update_progress(&path1, 250);
        assert_eq!(tracker.get_global_progress(), 0.25); // 250/1000

        // 更新文件1进度到100%
        tracker.update_progress(&path1, 500);
        assert_eq!(tracker.get_global_progress(), 0.5); // 500/1000

        // 完成文件1
        tracker.complete_file(&path1);
        assert_eq!(tracker.get_global_progress(), 0.5); // 500/1000 (不变)

        // 完成文件2
        tracker.complete_file(&path2);
        assert_eq!(tracker.get_global_progress(), 1.0); // 1000/1000
    }

    #[test]
    fn test_progress_tracker_reset() {
        let tracker = ProgressTracker::new();
        let path1 = PathBuf::from("/test/file1.txt");

        tracker.set_total(1000);
        tracker.start_file(path1.clone(), 1000);
        tracker.complete_file(&path1);

        assert_eq!(tracker.get_global_progress(), 1.0);

        tracker.reset();

        assert_eq!(tracker.get_global_progress(), 0.0);
        assert_eq!(tracker.get_processed_bytes(), 0);
        assert_eq!(tracker.get_total_bytes(), 0);
        assert_eq!(tracker.get_in_progress_count(), 0);
    }

    #[test]
    fn test_progress_tracker_zero_total() {
        let tracker = ProgressTracker::new();
        assert_eq!(tracker.get_global_progress(), 0.0);
    }
}
