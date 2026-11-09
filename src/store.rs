//! お気に入り・ログファイル (F-05 / F-09)。

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// `~/.config/vpngate-gui/favorites.json`。
pub fn favorites_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.config",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        )
    });
    PathBuf::from(base).join("vpngate-gui").join("favorites.json")
}

/// `~/.cache/vpngate-gui/vpngate.log`。
pub fn log_path() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.cache",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        )
    });
    PathBuf::from(base).join("vpngate-gui").join("vpngate.log")
}

/// お気に入り(ホスト名集合)の読み込み。無ければ空。
pub fn load_favorites() -> HashSet<String> {
    let path = favorites_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// お気に入りの保存。
pub fn save_favorites(favs: &HashSet<String>) -> Result<()> {
    let path = favorites_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("設定目录の作成に失敗")?;
    }
    let mut v: Vec<String> = favs.iter().cloned().collect();
    v.sort();
    let text = serde_json::to_string_pretty(&v).context("お気に入りの変換に失敗")?;
    std::fs::write(&path, text).context("お気に入りの保存に失敗")?;
    Ok(())
}

/// ログファイルへ追記 (失敗しても無視する)。
pub fn append_log_file(line: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// タイムスタンプ付きログ行を作る。
pub fn stamped(message: &str) -> String {
    let now = chrono::Local::now().format("%H:%M:%S");
    format!("[{now}] {message}")
}
