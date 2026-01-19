use crossbeam_channel::{Receiver, Sender, bounded};
use std::fs;
use std::mem;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::worker::UiMessage;

#[cfg_attr(test, derive(Debug))]
pub enum ScannerMessage {
    Scan(Vec<PathBuf>),
}

pub struct FileScanner {
    tx: Sender<ScannerMessage>,
}

impl FileScanner {
    pub fn spawn(ui_tx: Sender<UiMessage>) -> Self {
        let (tx, rx) = bounded(32);

        thread::spawn(move || {
            Self::run(rx, ui_tx);
        });

        Self { tx }
    }

    pub fn scan(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(ScannerMessage::Scan(paths));
    }

    fn run(rx: Receiver<ScannerMessage>, ui_tx: Sender<UiMessage>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                ScannerMessage::Scan(paths) => {
                    for path in paths {
                        Self::scan_path(&path, &ui_tx);
                    }
                }
            }
        }
    }

    fn scan_path(root: &PathBuf, ui_tx: &Sender<UiMessage>) {
        if root.is_file() {
            if let Ok(metadata) = fs::metadata(root) {
                let _ = ui_tx.send(UiMessage::FilesDiscovered(vec![(
                    root.clone(),
                    metadata.len(),
                )]));
            }
            return;
        }

        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| !s.starts_with('.'))
                    .unwrap_or(false)
            });

        let mut batch = Vec::with_capacity(100);
        let mut last_send = Instant::now();

        for entry in walker {
            match entry {
                Ok(entry) if entry.file_type().is_file() => {
                    let path = entry.path().to_path_buf();

                    match entry.metadata() {
                        Ok(metadata) => {
                            batch.push((path, metadata.len()));
                        }
                        Err(e) => {
                            eprintln!(
                                "[Scanner] 跳过文件（无法读取元数据）: {} - {}",
                                path.display(),
                                e
                            );
                        }
                    }

                    if batch.len() >= 100 || last_send.elapsed() >= Duration::from_millis(50) {
                        let _ = ui_tx.send(UiMessage::FilesDiscovered(mem::take(&mut batch)));
                        last_send = Instant::now();

                        thread::yield_now();
                    }
                }
                Err(e) => {
                    let path_str = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "未知路径".to_string());
                    eprintln!("[Scanner] 遍历错误: {} - {}", path_str, e);
                }
                _ => {
                    // 不是文件（目录、符号链接等），跳过
                }
            }
        }

        if !batch.is_empty() {
            let _ = ui_tx.send(UiMessage::FilesDiscovered(batch));
        }
    }
}
