#![cfg_attr(windows, windows_subsystem = "windows")]
#![warn(clippy::all, clippy::pedantic)]

mod cache;
mod engine;
mod error;
mod font;
mod hash;
mod progress;
mod scanner; // 新增模块
mod ui;
mod utils;
mod worker;

use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    // 解析命令行参数，仅检查存在性，不展开文件夹
    let initial_paths: Vec<PathBuf> = std::env::args()
        .skip(1)
        .filter_map(|arg| {
            let path = PathBuf::from(&arg);
            if path.exists() {
                Some(path)
            } else {
                eprintln!("警告: 路径不存在，跳过: {arg}");
                None
            }
        })
        .collect();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_icon(egui::IconData::default()),
        ..Default::default()
    };

    eframe::run_native(
        "TurboHash",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // 直接传递路径，UI 初始化后会调用 Scanner 异步扫描
            Ok(Box::new(ui::TurboHashApp::new(cc, initial_paths)?))
        }),
    )
}