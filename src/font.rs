// 零体积字体加载模块

use egui::FontDefinitions;
use std::path::PathBuf;

use crate::error::{HashError, HashResult};

#[cfg(target_os = "macos")]
use dirs::home_dir;

#[cfg(target_os = "linux")]
use dirs::home_dir;

pub fn load_chinese_font(fonts: &mut FontDefinitions) -> HashResult<()> {
    let font_paths = get_system_chinese_fonts();

    for font_path in font_paths {
        if let Ok(font_data) = std::fs::read(&font_path) {
            fonts.font_data.insert(
                "chinese".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(font_data)),
            );

            for (_family, font_ids) in fonts.families.iter_mut() {
                font_ids.insert(0, "chinese".to_owned());
            }

            return Ok(());
        }
    }

    Err(HashError::FontLoadFailed(
        "未找到可用的中文字体".to_string(),
    ))
}

fn get_system_chinese_fonts() -> Vec<PathBuf> {
    let mut fonts = Vec::new();

    #[cfg(target_os = "windows")]
    {
        fonts.extend(get_windows_chinese_fonts());
    }

    #[cfg(target_os = "macos")]
    {
        fonts.extend(get_macos_chinese_fonts());
    }

    #[cfg(target_os = "linux")]
    {
        fonts.extend(get_linux_chinese_fonts());
    }

    fonts
}

#[cfg(target_os = "windows")]
fn get_windows_chinese_fonts() -> Vec<PathBuf> {
    let mut fonts = Vec::new();

    let font_dir = std::env::var("SYSTEMROOT")
        .or_else(|_| std::env::var("WINDIR"))
        .map(|p| PathBuf::from(p).join("Fonts"))
        .unwrap_or_else(|_| PathBuf::from(r"C:\Windows\Fonts"));

    fonts.push(font_dir.join("msyh.ttc"));
    fonts.push(font_dir.join("msyhbd.ttc"));
    fonts.push(font_dir.join("msyhl.ttc"));
    fonts.push(font_dir.join("msjh.ttc"));
    fonts.push(font_dir.join("msjhbd.ttc"));
    fonts.push(font_dir.join("simhei.ttf"));
    fonts.push(font_dir.join("simsun.ttc"));

    fonts
}

#[cfg(target_os = "macos")]
fn get_macos_chinese_fonts() -> Vec<PathBuf> {
    let mut fonts = Vec::new();

    fonts.push(PathBuf::from("/System/Library/Fonts/PingFang.ttc"));
    fonts.push(PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"));
    fonts.push(PathBuf::from("/System/Library/Fonts/STHeiti Medium.ttc"));

    if let Some(home) = home_dir() {
        let user_font_dir = home.join("Library/Fonts");
        fonts.push(user_font_dir.join("PingFang.ttc"));
    }

    fonts
}

#[cfg(target_os = "linux")]
fn get_linux_chinese_fonts() -> Vec<PathBuf> {
    let mut fonts = Vec::new();

    let font_dirs = [
        "/usr/share/fonts",
        "/usr/share/fonts/truetype",
        "/usr/share/fonts/opentype",
        "/usr/local/share/fonts",
        "~/.local/share/fonts",
        "~/.fonts",
    ];

    let font_names = [
        "NotoSansCJK-Regular.ttc",
        "NotoSansCJK-sc-Regular.otf",
        "NotoSansCJKsc-Regular.otf",
        "wqy-zenhei.ttc",
        "wqy-microhei.ttc",
    ];

    for dir in &font_dirs {
        let dir_path = if dir.starts_with('~') {
            if let Some(home) = home_dir() {
                home.join(dir.strip_prefix("~/").unwrap_or(dir))
            } else {
                continue;
            }
        } else {
            PathBuf::from(dir)
        };

        for name in &font_names {
            fonts.push(dir_path.join(name));
        }

        if let Ok(entries) = std::fs::read_dir(&dir_path) {
            for entry in entries.flatten() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_dir() {
                        for name in &font_names {
                            fonts.push(entry.path().join(name));
                        }
                    }
                }
            }
        }
    }

    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_system_chinese_fonts() {
        let fonts = get_system_chinese_fonts();
        assert!(!fonts.is_empty());
    }

    #[test]
    fn test_load_chinese_font() {
        let mut fonts = FontDefinitions::default();
        let result = load_chinese_font(&mut fonts);
        let _ = result;
    }
}
