// 工作线程管理模块

#![allow(clippy::cast_possible_truncation)]

use crossbeam_channel::{Receiver, Sender, bounded};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::cache::{CacheEntry, HashCache, get_file_modified_time};
use crate::engine::{ProgressUpdate, compute_all_hashes_cached, compute_xxhash3_only};
use crate::scanner::FileScanner;

/// UI发送给工作线程的消息
#[cfg_attr(test, derive(Debug))]
pub enum WorkerMessage {
    Compute(Vec<PathBuf>),
    Scan(Vec<PathBuf>),
    SaveCache(Vec<CacheEntry>),
    Cancel,
}

/// 工作线程发送给UI的消息
#[cfg_attr(test, derive(Debug))]
pub enum UiMessage {
    FileStarted {
        path: PathBuf,
    },
    // 仅用于更新UI显示的哈希值，不作为完成信号
    Xxhash3Computed {
        path: PathBuf,
        xxhash3: String,
    },
    FileCompleted {
        path: PathBuf,
        crc32: String,
        md5: String,
        sha1: String,
        xxhash3: String, // 确保包含所有数据
        duration_ms: u64,
        modified_time: u64,
        file_size: u64,
        from_cache: bool, // 明确标记是否来自缓存
    },
    FileFailed {
        path: PathBuf,
    },
    FilesDiscovered(Vec<(PathBuf, u64)>), // 批量文件发现 (路径, 大小)
    Progress {
        path: PathBuf,
        processed: u64,
        total: u64,
    },
    CacheSaved, // 缓存保存完成通知
    AllCompleted,
}

enum MultiplexorMessage {
    Register {
        path: PathBuf,
        progress_rx: Receiver<ProgressUpdate>,
    },
}

pub struct WorkerThread {}

impl WorkerThread {
    pub fn spawn(
        cache: Arc<Mutex<HashCache>>,
    ) -> (Self, Sender<WorkerMessage>, Receiver<UiMessage>) {
        let (worker_tx, worker_rx) = bounded(16);
        let (ui_tx, ui_rx) = bounded(64);
        let (multiplexor_tx, multiplexor_rx) = bounded(128);

        let ui_tx_for_multiplexor = ui_tx.clone();
        thread::spawn(move || {
            Self::run_progress_multiplexor(multiplexor_rx, ui_tx_for_multiplexor);
        });

        let scanner = FileScanner::spawn(ui_tx.clone());

        thread::spawn(move || {
            Self::run(worker_rx, ui_tx, multiplexor_tx, cache, scanner);
        });

        (WorkerThread {}, worker_tx, ui_rx)
    }

    fn run_progress_multiplexor(
        multiplexor_rx: Receiver<MultiplexorMessage>,
        ui_tx: Sender<UiMessage>,
    ) {
        let mut progress_channels: HashMap<PathBuf, Receiver<ProgressUpdate>> = HashMap::new();
        // 限制进度更新频率：每16ms（约60fps）才发送一次UI更新
        let mut last_ui_update = std::time::Instant::now();

        loop {
            // 处理新注册
            while let Ok(msg) = multiplexor_rx.try_recv() {
                match msg {
                    MultiplexorMessage::Register { path, progress_rx } => {
                        progress_channels.insert(path, progress_rx);
                    }
                }
            }

            if progress_channels.is_empty() {
                match multiplexor_rx.recv() {
                    Ok(MultiplexorMessage::Register { path, progress_rx }) => {
                        progress_channels.insert(path, progress_rx);
                    }
                    Err(_) => return,
                }
            }

            let mut completed_paths = Vec::new();
            let should_send_update = last_ui_update.elapsed().as_millis() >= 32; // 降至30fps以减轻UI压力

            for (path, progress_rx) in &progress_channels {
                match progress_rx.try_recv() {
                    Ok(progress) => {
                        if should_send_update {
                            let _ = ui_tx.send(UiMessage::Progress {
                                path: path.clone(),
                                processed: progress.processed,
                                total: progress.total,
                            });
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        completed_paths.push(path.clone());
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }
            }

            if should_send_update {
                last_ui_update = std::time::Instant::now();
            }

            for path in completed_paths {
                progress_channels.remove(&path);
            }

            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn run(
        worker_rx: Receiver<WorkerMessage>,
        ui_tx: Sender<UiMessage>,
        multiplexor_tx: Sender<MultiplexorMessage>,
        cache: Arc<Mutex<HashCache>>,
        scanner: FileScanner,
    ) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get())
            .thread_name(|index| format!("turbohash-worker-{index}"))
            .stack_size(2 * 1024 * 1024)
            .build_global()
            .ok();

        while let Ok(msg) = worker_rx.recv() {
            match msg {
                WorkerMessage::Compute(files) => {
                    // 启动独立的计算线程，不阻塞 Worker 接收其他消息（如 Scan, SaveCache）
                    let ui_tx = ui_tx.clone();
                    let multiplexor_tx = multiplexor_tx.clone();
                    let cache = cache.clone();

                    thread::spawn(move || {
                        Self::compute_batch(files, &ui_tx, &multiplexor_tx, &cache);
                    });
                }
                WorkerMessage::Scan(paths) => {
                    scanner.scan(paths);
                }
                WorkerMessage::SaveCache(entries) => {
                    let cache = cache.clone();
                    let ui_tx = ui_tx.clone();
                    // 在独立线程中保存，避免阻塞 Worker 循环或计算
                    thread::spawn(move || {
                        if let Ok(guard) = cache.lock() {
                            if let Err(e) = guard.save_entries_batch(&entries) {
                                eprintln!("[Worker] 保存缓存失败: {}", e);
                            } else {
                                let _ = ui_tx.send(UiMessage::CacheSaved);
                            }
                        }
                    });
                }
                WorkerMessage::Cancel => {
                    // No-op for API compatibility
                }
            }
        }
    }

    fn compute_batch(
        files: Vec<PathBuf>,
        ui_tx: &Sender<UiMessage>,
        multiplexor_tx: &Sender<MultiplexorMessage>,
        cache: &Arc<Mutex<HashCache>>,
    ) {
        use rayon::prelude::*;

        let (buffer_size, mmap_chunk_size) = if let Ok(cache_guard) = cache.lock() {
            (
                cache_guard.get_buffer_size(),
                cache_guard.get_mmap_chunk_size(),
            )
        } else {
            (256 * 1024, 4 * 1024 * 1024)
        };

        let cache_map: HashMap<PathBuf, Option<CacheEntry>> = if let Ok(cache_guard) = cache.lock()
        {
            let paths: Vec<&PathBuf> = files.iter().collect();
            let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
            cache_guard
                .get_by_paths_batch(&path_refs)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };

        files.par_iter().for_each(|path| {
            let start = std::time::Instant::now();
            let _ = ui_tx.send(UiMessage::FileStarted { path: path.clone() });

            let (progress_tx, progress_rx) = bounded(32);
            let _ = multiplexor_tx.send(MultiplexorMessage::Register {
                path: path.clone(),
                progress_rx,
            });

            let (file_size, modified_time, metadata_valid) =
                if let Ok(metadata) = fs::metadata(path) {
                    if let Ok(mtime) = get_file_modified_time(path) {
                        (metadata.len(), mtime, true)
                    } else {
                        (metadata.len(), 0, false)
                    }
                } else {
                    let _ = ui_tx.send(UiMessage::FileFailed { path: path.clone() });
                    return;
                };

            let cache_entry = cache_map.get(path).and_then(|entry| entry.as_ref());

            if let Some(entry) = cache_entry {
                if metadata_valid
                    && HashCache::is_valid_with_metadata(entry, file_size, modified_time)
                {
                    match compute_xxhash3_only(
                        path,
                        Some(&progress_tx),
                        buffer_size,
                        mmap_chunk_size,
                    ) {
                        Ok((computed_xxhash3, _)) => {
                            if HashCache::validate_cache_integrity(
                                entry,
                                &computed_xxhash3,
                                file_size,
                                modified_time,
                            ) {
                                if let Ok(cache_guard) = cache.lock() {
                                    if let Ok(true) = cache_guard.verify_cached_hashes(entry) {
                                        eprintln!("[Cache] ✓ 缓存命中: {}", path.display());
                                        let _ = ui_tx.send(UiMessage::Xxhash3Computed {
                                            path: path.clone(),
                                            xxhash3: computed_xxhash3.clone(),
                                        });
                                        let _ = ui_tx.send(UiMessage::FileCompleted {
                                            path: path.clone(),
                                            crc32: entry.crc32.clone(),
                                            md5: entry.md5.clone(),
                                            sha1: entry.sha1.clone(),
                                            xxhash3: computed_xxhash3,
                                            duration_ms: start.elapsed().as_millis() as u64,
                                            modified_time,
                                            file_size,
                                            from_cache: true,
                                        });
                                        return;
                                    }
                                }
                            }

                            eprintln!("[Cache] ✗ 缓存失效: {}", path.display());
                            if let Ok(cache_guard) = cache.lock() {
                                let _ = cache_guard.invalidate_entry(path);
                            }
                        }
                        Err(_e) => {
                            let _ = ui_tx.send(UiMessage::FileFailed { path: path.clone() });
                            return;
                        }
                    }
                }
            }

            match compute_all_hashes_cached(path, Some(&progress_tx), buffer_size, mmap_chunk_size)
            {
                Ok((crc32, md5, sha1, xxhash3, computed_file_size)) => {
                    let duration = start.elapsed().as_millis() as u64;

                    let _ = ui_tx.send(UiMessage::Xxhash3Computed {
                        path: path.clone(),
                        xxhash3: xxhash3.clone(),
                    });

                    let _ = ui_tx.send(UiMessage::FileCompleted {
                        path: path.clone(),
                        crc32,
                        md5,
                        sha1,
                        xxhash3,
                        duration_ms: duration,
                        modified_time,
                        file_size: computed_file_size,
                        from_cache: false,
                    });
                }
                Err(_e) => {
                    let _ = ui_tx.send(UiMessage::FileFailed { path: path.clone() });
                }
            }
        });

        let _ = ui_tx.send(UiMessage::AllCompleted);
    }
}
