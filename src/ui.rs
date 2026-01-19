// GUI主逻辑模块

use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dunce;
use egui::{self, CentralPanel, ScrollArea, TopBottomPanel, Widget};
use egui_extras::{Column, TableBuilder};

use crate::cache::{CacheConfig, CacheEntry, HashCache};
use crate::error::{HashError, HashResult};
use crate::font::load_chinese_font;
use crate::progress::ProgressTracker;
use crate::utils::format_duration;
use crate::worker::{UiMessage, WorkerMessage, WorkerThread};

/// 文件状态
#[derive(Debug, Clone)]
pub enum FileStatus {
    Pending,
    Computing,
    Completed,
    Failed,
    Cancelled,
}

/// 文件项
#[derive(Debug, Clone)]
pub struct FileItem {
    pub path: PathBuf,
    pub size: u64,
    pub size_str: String,
    pub status: FileStatus,
    pub crc32: String,
    pub md5: String,
    pub sha1: String,
    pub xxhash3: String,
    pub progress: f64,
    pub from_cache: bool,
    computation_start_time: Option<std::time::Instant>,
    computation_duration_ms: Option<u64>,
}

impl FileItem {
    // 现在接收 size，不再进行 IO 操作
    pub fn new(path: PathBuf, size: u64) -> Self {
        let size_str = humansize::format_size(size, humansize::BINARY);

        Self {
            path,
            size,
            size_str,
            status: FileStatus::Pending,
            crc32: String::new(),
            md5: String::new(),
            sha1: String::new(),
            xxhash3: String::new(),
            progress: 0.0,
            from_cache: false,
            computation_start_time: None,
            computation_duration_ms: None,
        }
    }

    pub fn filename(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("无效文件名")
            .to_string()
    }

    pub fn status_icon(&self) -> &str {
        match &self.status {
            FileStatus::Pending => "等待",
            FileStatus::Computing => "计算",
            FileStatus::Completed if self.from_cache => "缓存",
            FileStatus::Completed => "完成",
            FileStatus::Failed => "失败",
            FileStatus::Cancelled => "取消",
        }
    }

    pub fn duration_str(&self) -> String {
        match self.computation_duration_ms {
            Some(ms) => format_duration(ms),
            None => String::from("-"),
        }
    }
}

/// TurboHash主应用
pub struct TurboHashApp {
    files: Vec<FileItem>,
    file_index: HashMap<PathBuf, usize>,
    ui_rx: Receiver<UiMessage>,       // 必须始终存在
    worker_tx: Sender<WorkerMessage>, // 必须始终存在
    progress_tracker: Option<ProgressTracker>,
    global_progress: f64,
    total_size: u64,
    processed_size: u64,
    is_computing: bool,
    auto_compute_enabled: bool,
    last_file_add_time: Option<std::time::Instant>,
    debounce_duration_ms: u64,
    auto_compute_scheduled: bool,
    cache: Arc<Mutex<HashCache>>, // 仅用于配置读取，主要操作移至 worker
    cache_config: CacheConfig,
    show_cache_settings: bool,
    batch_start_time: Option<std::time::Instant>,
    batch_total_duration_ms: u64,
    cache_operation_message: Option<String>,
    uppercase_display: bool,
    clipboard_toast: Option<(String, std::time::Instant)>,
    pending_cache_entries: Vec<CacheEntry>,
}

impl TurboHashApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_files: Vec<PathBuf>) -> HashResult<Self> {
        let mut fonts = egui::FontDefinitions::default();
        load_chinese_font(&mut fonts).ok();
        cc.egui_ctx.set_fonts(fonts);

        let cache_config = CacheConfig::default();
        let exe_path =
            std::env::current_exe().map_err(|e| HashError::Io(e, PathBuf::from("current_exe")))?;
        let cache_path = exe_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("hash_cache.db");

        // 初始化缓存和 Worker
        let (cache, cache_config) = match HashCache::new(&cache_path, cache_config.clone()) {
            Ok(c) => {
                let saved_config = c.load_cache_config();
                match saved_config {
                    Ok(config) => (Arc::new(Mutex::new(c)), config),
                    Err(_) => {
                        let auto_config = crate::engine::detect_optimal_config();
                        (Arc::new(Mutex::new(c)), auto_config)
                    }
                }
            }
            Err(e) => {
                eprintln!("[UI] 缓存初始化失败: {}", e);
                // 降级到内存缓存
                match HashCache::new(std::path::Path::new(":memory:"), cache_config.clone()) {
                    Ok(mem_cache) => (Arc::new(Mutex::new(mem_cache)), cache_config.clone()),
                    Err(mem_err) => {
                        eprintln!("[UI] 内存缓存初始化也失败: {}, 应用程序无法继续", mem_err);
                        std::process::exit(1);
                    }
                }
            }
        };

        let (_worker, worker_tx, ui_rx) = WorkerThread::spawn(cache.clone());
        let uppercase_display = cache_config.uppercase_display;
        let auto_compute_enabled = cache_config.auto_compute_enabled;

        let mut app = Self {
            files: Vec::new(),
            file_index: HashMap::new(),
            ui_rx,
            worker_tx,
            progress_tracker: None,
            global_progress: 0.0,
            total_size: 0,
            processed_size: 0,
            is_computing: false,
            auto_compute_enabled,
            last_file_add_time: None,
            debounce_duration_ms: 500,
            auto_compute_scheduled: false,
            cache,
            cache_config,
            show_cache_settings: false,
            batch_start_time: None,
            batch_total_duration_ms: 0,
            cache_operation_message: None,
            uppercase_display,
            clipboard_toast: None,
            pending_cache_entries: Vec::new(),
        };

        if !initial_files.is_empty() {
            app.add_files(initial_files);
        }

        Ok(app)
    }

    pub fn add_files(&mut self, paths: Vec<PathBuf>) {
        // 仅仅是将路径发送给 Scanner，完全非阻塞
        let _ = self.worker_tx.send(WorkerMessage::Scan(paths));
    }

    fn open_file_dialog(&mut self) {
        use rfd::FileDialog;
        // 注意：FileDialog 可能会阻塞，通常在主线程调用是可以接受的，因为它就是模态对话框
        if let Some(paths) = FileDialog::new()
            .set_title("选择要计算哈希的文件")
            .pick_files()
        {
            let path_bufs: Vec<PathBuf> = paths.into_iter().map(|p| p.into()).collect();
            self.add_files(path_bufs);
        }
    }

    fn open_folder_dialog(&mut self) {
        use rfd::FileDialog;
        if let Some(folder_path) = FileDialog::new()
            .set_title("选择要计算哈希的文件夹")
            .pick_folder()
        {
            self.add_files(vec![folder_path]);
        }
    }

    pub fn clear_files(&mut self) {
        self.files.clear();
        self.file_index.clear();
        self.total_size = 0;
        self.processed_size = 0;
        self.global_progress = 0.0;
        self.batch_start_time = None;
        self.batch_total_duration_ms = 0;
        self.last_file_add_time = None;
        self.auto_compute_scheduled = false;
        self.clipboard_toast = None;
        if let Some(tracker) = &self.progress_tracker {
            tracker.reset();
        }
        self.progress_tracker = None;
        self.pending_cache_entries.clear();
    }

    fn finalize_batch(&mut self) {
        if let Some(start_time) = self.batch_start_time {
            self.batch_total_duration_ms = start_time.elapsed().as_millis() as u64;
            self.batch_start_time = None;
        }
    }

    pub fn start_computing(&mut self) {
        if self.files.is_empty() {
            return;
        }

        if self.batch_start_time.is_none() {
            self.batch_start_time = Some(std::time::Instant::now());
        }

        // 重新计算未完成文件的总大小
        let pending_files: Vec<_> = self
            .files
            .iter()
            .filter(|f| matches!(f.status, FileStatus::Pending))
            .collect();

        let pending_paths: Vec<_> = pending_files.iter().map(|f| f.path.clone()).collect();
        let pending_size: u64 = pending_files.iter().map(|f| f.size).sum();

        if pending_paths.is_empty() {
            return;
        }

        self.progress_tracker = Some(ProgressTracker::new());
        if let Some(tracker) = &self.progress_tracker {
            tracker.set_total(pending_size);
        }

        self.processed_size = 0; // 批次内已处理

        self.is_computing = true;
        let _ = self.worker_tx.send(WorkerMessage::Compute(pending_paths));
    }

    pub fn stop_computing(&mut self) {
        let _ = self.worker_tx.send(WorkerMessage::Cancel);
        self.is_computing = false;

        for file in &mut self.files {
            if matches!(file.status, FileStatus::Computing) {
                file.status = FileStatus::Cancelled;
            }
        }

        if let Some(tracker) = &self.progress_tracker {
            tracker.reset();
        }
        self.progress_tracker = None;

        self.finalize_batch();
        self.last_file_add_time = None;
        self.auto_compute_scheduled = false;
    }

    fn process_messages(&mut self, ctx: &egui::Context) {
        const MAX_MESSAGES_PER_FRAME: usize = 100; // 增加每帧处理量
        let mut should_finalize_batch = false;
        let mut processed_count = 0;
        let mut new_files_added = false;

        while let Ok(msg) = self.ui_rx.try_recv() {
            if processed_count >= MAX_MESSAGES_PER_FRAME {
                ctx.request_repaint(); // 还有消息，下一帧继续
                break;
            }
            processed_count += 1;

            match msg {
                UiMessage::FilesDiscovered(batch) => {
                    for (path, size) in batch {
                        if !self.file_index.contains_key(&path) {
                            let item = FileItem::new(path.clone(), size);
                            let idx = self.files.len();
                            self.file_index.insert(path, idx);
                            self.files.push(item);
                            self.total_size += size;
                            new_files_added = true;
                        }
                    }
                }
                UiMessage::FileStarted { path } => {
                    if let Some(&idx) = self.file_index.get(&path) {
                        let file = &mut self.files[idx];
                        file.status = FileStatus::Computing;
                        file.computation_start_time = Some(std::time::Instant::now());
                        file.progress = 0.0;

                        if let Some(tracker) = &self.progress_tracker {
                            tracker.start_file(path.clone(), file.size);
                        }
                    }
                }
                UiMessage::Xxhash3Computed { path, xxhash3 } => {
                    if let Some(&idx) = self.file_index.get(&path) {
                        let file = &mut self.files[idx];
                        file.xxhash3 = xxhash3;
                    }
                }
                UiMessage::FileCompleted {
                    path,
                    crc32,
                    md5,
                    sha1,
                    xxhash3,
                    duration_ms,
                    modified_time,
                    file_size,
                    from_cache,
                } => {
                    if let Some(&idx) = self.file_index.get(&path) {
                        let file = &mut self.files[idx];

                        file.status = FileStatus::Completed;
                        file.crc32 = crc32.clone();
                        file.md5 = md5.clone();
                        file.sha1 = sha1.clone();
                        file.xxhash3 = xxhash3.clone(); // 确保更新
                        file.progress = 1.0;
                        file.computation_duration_ms = Some(duration_ms);
                        file.computation_start_time = None;
                        file.from_cache = from_cache;

                        self.processed_size += file.size;

                        if let Some(tracker) = &self.progress_tracker {
                            tracker.complete_file(&path);
                            self.global_progress = tracker.get_global_progress();
                        }

                        // 如果不是来自缓存，加入待保存队列
                        if !from_cache {
                            use std::time::{SystemTime, UNIX_EPOCH};
                            let entry = CacheEntry {
                                path: path.clone(),
                                file_size,
                                modified_time,
                                cached_at: SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or(std::time::Duration::ZERO)
                                    .as_secs(),
                                xxhash3: xxhash3.clone(),
                                crc32,
                                md5,
                                sha1,
                            };
                            self.pending_cache_entries.push(entry);
                        }
                    }
                }
                UiMessage::FileFailed { path } => {
                    if let Some(&idx) = self.file_index.get(&path) {
                        let file = &mut self.files[idx];
                        file.status = FileStatus::Failed;
                        file.computation_start_time = None;
                    }
                }
                UiMessage::Progress {
                    path,
                    processed,
                    total,
                } => {
                    if let Some(&idx) = self.file_index.get(&path) {
                        let file = &mut self.files[idx];
                        if matches!(file.status, FileStatus::Completed) {
                            continue;
                        }
                        if total > 0 {
                            file.progress = processed as f64 / total as f64;
                        }
                        if let Some(tracker) = &self.progress_tracker {
                            tracker.update_progress(&path, processed);
                            self.global_progress = tracker.get_global_progress();
                        }
                    }
                }
                UiMessage::CacheSaved => {
                    // 可以在这里显示保存成功的提示
                }
                UiMessage::AllCompleted => {
                    self.is_computing = false;
                    self.global_progress = 1.0;
                    self.auto_compute_scheduled = false;
                    should_finalize_batch = true;
                    if let Some(tracker) = &self.progress_tracker {
                        tracker.reset();
                    }
                    self.progress_tracker = None;

                    if !self.pending_cache_entries.is_empty() {
                        let _ = self.worker_tx.send(WorkerMessage::SaveCache(std::mem::take(
                            &mut self.pending_cache_entries,
                        )));
                    }
                }
            }
        }

        if !self.pending_cache_entries.is_empty() {
            let should_flush = self.pending_cache_entries.len() >= 50;
            if should_flush {
                let _ = self.worker_tx.send(WorkerMessage::SaveCache(std::mem::take(
                    &mut self.pending_cache_entries,
                )));
            }
        }

        if new_files_added && self.auto_compute_enabled {
            self.schedule_auto_compute();
        }

        if should_finalize_batch {
            self.finalize_batch();
        }
    }

    fn schedule_auto_compute(&mut self) {
        self.last_file_add_time = Some(std::time::Instant::now());
        self.auto_compute_scheduled = true;
    }

    fn check_and_execute_auto_compute(&mut self) {
        if !self.auto_compute_scheduled {
            return;
        }

        if let Some(last_add_time) = self.last_file_add_time {
            let elapsed = last_add_time.elapsed().as_millis() as u64;

            if elapsed >= self.debounce_duration_ms {
                self.start_computing();
                self.last_file_add_time = None;
                self.auto_compute_scheduled = false;
            }
        }
    }

    fn show_hash_cell(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        hash_value: &str,
        unique_id: &str,
    ) -> egui::Response {
        if hash_value.is_empty() {
            ui.label(egui::RichText::new("-").weak().italics())
        } else {
            let display_value = if self.uppercase_display {
                hash_value.to_uppercase()
            } else {
                hash_value.to_string()
            };

            let show_toast = self
                .clipboard_toast
                .as_ref()
                .map_or(false, |(id, _)| id == unique_id);
            let label_text = if show_toast {
                egui::RichText::new("已复制到剪贴板").color(egui::Color32::GREEN)
            } else {
                egui::RichText::new(&display_value).monospace()
            };

            let response = ui.label(label_text).on_hover_text("点击复制");

            if response.hovered() {
                ui.painter().rect_filled(
                    response.rect,
                    egui::CornerRadius::same(4),
                    egui::Color32::from_rgba_premultiplied(60, 60, 60, 50),
                );
            }

            if response.clicked() {
                ctx.copy_text(display_value.clone());
                self.clipboard_toast = Some((unique_id.to_string(), std::time::Instant::now()));
            }

            response
        }
    }

    fn render_settings_window(&mut self, ctx: &egui::Context) {
        // --- 点击外部关闭 (遮罩层) ---
        egui::Area::new("settings_backdrop".into())
            .fixed_pos(egui::pos2(0.0, 0.0))
            .order(egui::Order::Middle) // 位于窗口之下
            .show(ctx, |ui| {
                let screen_rect = ctx.viewport_rect();
                // 绘制半透明遮罩
                ui.painter().rect_filled(
                    screen_rect,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_black_alpha(100),
                );

                // 捕获点击事件
                let response = ui.allocate_rect(screen_rect, egui::Sense::click());
                if response.clicked() {
                    self.show_cache_settings = false;
                    self.cache_operation_message = None;
                }
            });

        let mut open = self.show_cache_settings;
        let mut config_changed = false;

        egui::Window::new("缓存设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.viewport_rect().center())
            .order(egui::Order::Foreground) // 位于遮罩之上
            .show(ctx, |ui| {
                if let Ok(cache_guard) = self.cache.lock() {
                    ui.add_space(8.0);

                    // --- 1. 性能模式 (Segmented Control) ---
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("🚀 性能模式").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new("调整 I/O 策略").weak().small());
                        });
                    });
                    ui.add_space(4.0);

                    let current_preset = if self.cache_config.buffer_size == 64 * 1024
                        && self.cache_config.mmap_chunk_size == 1024 * 1024
                    {
                        0 // 节能
                    } else if self.cache_config.buffer_size == 256 * 1024
                        && self.cache_config.mmap_chunk_size == 4 * 1024 * 1024
                    {
                        1 // 均衡
                    } else if self.cache_config.buffer_size == 1024 * 1024
                        && self.cache_config.mmap_chunk_size == 16 * 1024 * 1024
                    {
                        2 // 高性能
                    } else {
                        3 // 自定义
                    };

                    let mut selected_preset = current_preset;
                    ui.horizontal(|ui| {
                        ui.style_mut().spacing.item_spacing.x = 0.0;
                        // 简单的分段按钮样式
                        if ui
                            .selectable_label(selected_preset == 0, "🍃 节能")
                            .clicked()
                        {
                            selected_preset = 0;
                            config_changed = true;
                        }
                        if ui
                            .selectable_label(selected_preset == 1, "⚖️ 均衡")
                            .clicked()
                        {
                            selected_preset = 1;
                            config_changed = true;
                        }
                        if ui
                            .selectable_label(selected_preset == 2, "⚡ 高性能")
                            .clicked()
                        {
                            selected_preset = 2;
                            config_changed = true;
                        }
                    });

                    if config_changed && selected_preset != current_preset {
                        match selected_preset {
                            0 => {
                                self.cache_config.buffer_size = 64 * 1024;
                                self.cache_config.mmap_chunk_size = 1024 * 1024;
                            }
                            1 => {
                                self.cache_config.buffer_size = 256 * 1024;
                                self.cache_config.mmap_chunk_size = 4 * 1024 * 1024;
                            }
                            2 => {
                                self.cache_config.buffer_size = 1024 * 1024;
                                self.cache_config.mmap_chunk_size = 16 * 1024 * 1024;
                            }
                            _ => {}
                        }
                    }

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(16.0);

                    // --- 2. 详细设置 (Grid Layout) ---
                    egui::Grid::new("settings_grid")
                        .num_columns(2)
                        .spacing([24.0, 12.0])
                        .striped(false)
                        .show(ui, |ui| {
                            // Row 1: Buffer Size
                            ui.label("读取缓冲");
                            egui::ComboBox::from_id_salt("buf_size")
                                .selected_text(humansize::format_size(
                                    self.cache_config.buffer_size,
                                    humansize::BINARY,
                                ))
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.buffer_size,
                                            64 * 1024,
                                            "64 KB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.buffer_size,
                                            256 * 1024,
                                            "256 KB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.buffer_size,
                                            1024 * 1024,
                                            "1 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.buffer_size,
                                            2 * 1024 * 1024,
                                            "2 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.buffer_size,
                                            4 * 1024 * 1024,
                                            "4 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                });
                            ui.end_row();

                            // Row 2: MMAP Chunk
                            ui.label("内存映射");
                            egui::ComboBox::from_id_salt("mmap_size")
                                .selected_text(humansize::format_size(
                                    self.cache_config.mmap_chunk_size,
                                    humansize::BINARY,
                                ))
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.mmap_chunk_size,
                                            1024 * 1024,
                                            "1 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.mmap_chunk_size,
                                            4 * 1024 * 1024,
                                            "4 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.mmap_chunk_size,
                                            16 * 1024 * 1024,
                                            "16 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.mmap_chunk_size,
                                            64 * 1024 * 1024,
                                            "64 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                });
                            ui.end_row();

                            // Row 3: Min File Size
                            ui.label("缓存阈值");
                            egui::ComboBox::from_id_salt("min_file_size")
                                .selected_text(humansize::format_size(
                                    self.cache_config.min_file_size,
                                    humansize::BINARY,
                                ))
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.min_file_size,
                                            1024 * 1024,
                                            "1 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.min_file_size,
                                            10 * 1024 * 1024,
                                            "10 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.min_file_size,
                                            100 * 1024 * 1024,
                                            "100 MB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                    if ui
                                        .selectable_value(
                                            &mut self.cache_config.min_file_size,
                                            1024 * 1024 * 1024,
                                            "1 GB",
                                        )
                                        .changed()
                                    {
                                        config_changed = true;
                                    }
                                });
                            ui.end_row();

                            // Row 4: Retention
                            ui.label("保留期限");
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut self.cache_config.retention_days)
                                            .speed(1)
                                            .suffix(" 天"),
                                    )
                                    .changed()
                                {
                                    config_changed = true;
                                }
                                if self.cache_config.retention_days == 0 {
                                    ui.label(
                                        egui::RichText::new("(永久)")
                                            .color(egui::Color32::GOLD)
                                            .small(),
                                    );
                                }
                            });
                            ui.end_row();
                        });

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(16.0);

                    // --- 3. 维护操作 ---
                    ui.horizontal(|ui| {
                        if ui.button("🧹 清理过期").clicked() {
                            match cache_guard.cleanup_expired() {
                                Ok(count) => {
                                    self.cache_operation_message =
                                        Some(format!("已清理 {} 条", count))
                                }
                                Err(e) => {
                                    self.cache_operation_message = Some(format!("失败: {}", e))
                                }
                            }
                        }
                        if ui.button("🗑️ 清空所有").clicked() {
                            match cache_guard.clear_all() {
                                Ok(count) => {
                                    self.cache_operation_message =
                                        Some(format!("已清空 {} 条", count))
                                }
                                Err(e) => {
                                    self.cache_operation_message = Some(format!("失败: {}", e))
                                }
                            }
                        }

                        if let Some(msg) = &self.cache_operation_message {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(msg)
                                            .color(egui::Color32::LIGHT_BLUE)
                                            .small(),
                                    );
                                },
                            );
                        }
                    });

                    ui.add_space(8.0);

                    // 立即保存逻辑
                    if config_changed {
                        if let Err(e) = cache_guard.save_cache_config(&self.cache_config) {
                            eprintln!("保存配置失败: {}", e);
                        }
                    }
                }
            });
        self.show_cache_settings = open;
    }
}

impl eframe::App for TurboHashApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages(ctx);

        if let Some((_, instant)) = &self.clipboard_toast {
            if instant.elapsed().as_secs() >= 2 {
                self.clipboard_toast = None;
            }
        }

        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped_files.is_empty() {
            let mut paths = Vec::new();
            for file in dropped_files {
                if let Some(path) = file.path {
                    paths.push(path);
                }
            }
            if !paths.is_empty() {
                self.add_files(paths);
            }
        }

        self.check_and_execute_auto_compute();

        if self.is_computing || !self.ui_rx.is_empty() {
            ctx.request_repaint();
        }

        TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("TurboHash");
                ui.separator();

                if ui.button("添加文件").clicked() {
                    self.open_file_dialog();
                }

                if ui.button("添加文件夹").clicked() {
                    self.open_folder_dialog();
                }

                let clear_button_enabled = !self.is_computing;
                if ui
                    .add_enabled(clear_button_enabled, egui::Button::new("清空队列"))
                    .clicked()
                {
                    self.clear_files();
                }

                ui.separator();

                if ui.button("缓存设置").clicked() {
                    self.show_cache_settings = true;
                }

                ui.separator();

                if ui
                    .checkbox(&mut self.uppercase_display, "大写显示")
                    .changed()
                {
                    self.cache_config.uppercase_display = self.uppercase_display;
                    if let Ok(guard) = self.cache.lock() {
                        let _ = guard.save_cache_config(&self.cache_config);
                    }
                }

                if ui
                    .checkbox(&mut self.auto_compute_enabled, "自动计算")
                    .changed()
                {
                    self.cache_config.auto_compute_enabled = self.auto_compute_enabled;
                    if let Ok(guard) = self.cache.lock() {
                        let _ = guard.save_cache_config(&self.cache_config);
                    }
                    if !self.auto_compute_enabled {
                        self.last_file_add_time = None;
                        self.auto_compute_scheduled = false;
                    }
                }

                ui.separator();

                if self.is_computing {
                    if let Some(start_time) = self.batch_start_time {
                        let elapsed_ms = start_time.elapsed().as_millis() as u64;
                        ui.label(
                            egui::RichText::new(format!("已用时: {}", format_duration(elapsed_ms)))
                                .color(egui::Color32::GRAY),
                        );
                    }

                    if ui.button("停止").clicked() {
                        self.stop_computing();
                    }
                } else {
                    ui.add_enabled_ui(!self.files.is_empty(), |ui| {
                        if ui.button("开始计算").clicked() {
                            self.start_computing();
                            self.last_file_add_time = None;
                            self.auto_compute_scheduled = false;
                        }
                    });
                    if self.batch_total_duration_ms > 0 {
                        ui.label(format!(
                            "上次耗时: {}",
                            format_duration(self.batch_total_duration_ms)
                        ));
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("文件: {}", self.files.len()));
                });
            });
        });

        CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .striped(true)
                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                        .column(Column::exact(60.0))
                        .column(Column::initial(200.0).range(100.0..=400.0).clip(true))
                        .column(Column::exact(100.0))
                        .column(Column::exact(100.0))
                        .column(Column::exact(150.0))
                        .column(Column::initial(100.0).at_least(80.0).clip(true))
                        .column(Column::initial(290.0).range(180.0..=300.0).clip(true))
                        .column(Column::remainder().at_least(230.0).clip(true))
                        .header(30.0, |mut header| {
                            header.col(|ui| {
                                ui.strong("状态");
                            });
                            header.col(|ui| {
                                ui.strong("文件名");
                            });
                            header.col(|ui| {
                                ui.strong("大小");
                            });
                            header.col(|ui| {
                                ui.strong("耗时");
                            });
                            header.col(|ui| {
                                ui.strong("进度");
                            });
                            header.col(|ui| {
                                ui.strong("CRC32");
                            });
                            header.col(|ui| {
                                ui.strong("MD5");
                            });
                            header.col(|ui| {
                                ui.strong("SHA1");
                            });
                        })
                        .body(|body| {
                            body.rows(30.0, self.files.len(), |mut row| {
                                let idx = row.index();
                                if idx < self.files.len() {
                                    // 解决借用冲突：提前克隆需要的数据
                                    let (
                                        status_icon,
                                        filename,
                                        size_str,
                                        duration_str,
                                        progress,
                                        crc32,
                                        md5,
                                        sha1,
                                        path_str,
                                    ) = {
                                        let file = &self.files[idx];
                                        (
                                            file.status_icon().to_string(),
                                            file.filename(),
                                            file.size_str.clone(),
                                            file.duration_str(),
                                            file.progress,
                                            file.crc32.clone(),
                                            file.md5.clone(),
                                            file.sha1.clone(),
                                            dunce::simplified(&file.path).display().to_string(),
                                        )
                                    };

                                    row.col(|ui| {
                                        ui.label(status_icon);
                                    });
                                    row.col(|ui| {
                                        ui.label(filename);
                                    });
                                    row.col(|ui| {
                                        ui.label(size_str);
                                    });
                                    row.col(|ui| {
                                        ui.label(duration_str);
                                    });
                                    row.col(|ui| {
                                        egui::ProgressBar::new(progress as f32)
                                            .show_percentage()
                                            .ui(ui);
                                    });
                                    // 使用克隆的数据，不再持有 self.files 的借用
                                    row.col(|ui| {
                                        self.show_hash_cell(
                                            ui,
                                            ctx,
                                            &crc32,
                                            &format!("{}_crc32", path_str),
                                        );
                                    });
                                    row.col(|ui| {
                                        self.show_hash_cell(
                                            ui,
                                            ctx,
                                            &md5,
                                            &format!("{}_md5", path_str),
                                        );
                                    });
                                    row.col(|ui| {
                                        self.show_hash_cell(
                                            ui,
                                            ctx,
                                            &sha1,
                                            &format!("{}_sha1", path_str),
                                        );
                                    });
                                }
                            });
                        });
                    ui.add_space(40.0);
                });
        });
        // ... (Status panel code same as before, simplified to save space, but keeping key elements)
        TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("全局进度:");
                ui.add(egui::ProgressBar::new(self.global_progress as f32).show_percentage());
                ui.separator();
                ui.label(format!(
                    "已处理: {} / 总计: {}",
                    humansize::format_size(self.processed_size, humansize::BINARY),
                    humansize::format_size(self.total_size, humansize::BINARY)
                ));
            });
        });

        if self.show_cache_settings {
            self.render_settings_window(ctx);
        }
    }
}
