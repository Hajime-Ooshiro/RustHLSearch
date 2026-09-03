use chrono::Local;
use std::path::{Path, PathBuf};

/// 出力ファイルパスにタイムスタンプ (YYYYMMDD_HHMMSS) を挿入する
pub fn with_timestamp(path: &Path) -> PathBuf {
    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();

    let parent = path.parent();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let ext = path.extension().and_then(|s| s.to_str());

    let new_name = match ext {
        Some(ext) => format!("{}_{}.{}", stem, timestamp, ext),
        None => format!("{}_{}", stem, timestamp),
    };

    match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(new_name),
        _ => PathBuf::from(new_name),
    }
}
