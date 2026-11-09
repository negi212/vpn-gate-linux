//! nmcli による接続・切断 (F-06a / F-07)。
//!
//! 方針:
//! - Base64デコードは接続時のみ。`/tmp/vpngate-<host>.ovpn` を 0600 で作成。
//! - ovpn 内にパスなし `auth-user-pass` があれば資格情報ファイル(vpn/vpn)を
//!   作成して行を `auth-user-pass <cred>` に書き換える。
//! - `nmcli connection import type openvpn file <ovpn>` → `modify` で
//!   username/password を付与 → `nmcli connection up id <profile>`。
//! - 切断は `down` → `delete` → 一時ファイル削除。すべてバックグラウンドスレッド用。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// プロファイル名 (`VG-<HostName>` を nmcli 安全文字に寄せる)。
pub fn profile_name_for(hostname: &str) -> String {
    let safe: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let safe = safe.trim_matches(|c| c == '-' || c == '.').to_string();
    let short: String = safe.chars().take(48).collect();
    if short.is_empty() {
        "VG-unknown".to_string()
    } else {
        format!("VG-{short}")
    }
}

/// 一時 ovpn パス。
pub fn ovpn_temp_path(hostname: &str) -> PathBuf {
    let pid = std::process::id();
    let safe: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("vpngate-{safe}-{pid}.ovpn"))
}

/// 資格情報ファイルパス (ovpn と対になる)。
pub fn cred_temp_path(ovpn_path: &Path) -> PathBuf {
    let mut s = ovpn_path.as_os_str().to_owned();
    s.push(".cred");
    PathBuf::from(s)
}

/// Base64 → ovpnテキスト。auth-user-pass 対応込みで一時ファイルに書く。
/// 戻り値: (ovpnパス, 資格情報パス(あれば))。
pub fn write_ovpn_temp(config_base64: &str, hostname: &str) -> Result<(PathBuf, Option<PathBuf>)> {
    let raw = B64
        .decode(config_base64.trim())
        .context("Base64デコードに失敗しました。別サーバーで再試行してください")?;
    let mut text = String::from_utf8(raw)
        .context("ovpnデータがUTF-8ではありません。別サーバーで再試行してください")?;
    if text.trim().is_empty() {
        anyhow::bail!("ovpnデータが空でした。別サーバーで再試行してください");
    }

    let ovpn_path = ovpn_temp_path(hostname);
    let mut cred_path: Option<PathBuf> = None;

    // auth-user-pass の扱い:
    // - 行が `auth-user-pass` 単独 → 資格情報ファイルを作成してパス付きに書き換え
    // - 既にパス付き → そのまま
    // - 行自体がない → 何もしない(匿名接続可能なサーバー用)
    let needs_cred = text.lines().any(|l| l.trim() == "auth-user-pass");
    if needs_cred {
        let cred = cred_temp_path(&ovpn_path);
        std::fs::write(&cred, "vpn\nvpn\n").context("資格情報ファイルの作成に失敗")?;
        std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(0o600))
            .context("資格情報ファイルの権限設定に失敗")?;
        let rewritten: Vec<String> = text
            .lines()
            .map(|l| {
                if l.trim() == "auth-user-pass" {
                    format!("auth-user-pass {}", cred.display())
                } else {
                    l.to_string()
                }
            })
            .collect();
        text = rewritten.join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        cred_path = Some(cred);
    }

    std::fs::write(&ovpn_path, text.as_bytes()).context("一時ovpnの書き込みに失敗")?;
    std::fs::set_permissions(&ovpn_path, std::fs::Permissions::from_mode(0o600))
        .context("一時ovpnの権限設定(0600)に失敗")?;
    Ok((ovpn_path, cred_path))
}

/// コマンド実行ヘルパー (タイムアウト付き)。
fn run_cmd(prog: &str, args: &[&str], timeout: Duration) -> Result<String> {
    // `timeout` コマンドがあれば使い、無ければ素直に実行する。
    let use_timeout = Command::new("which")
        .arg("timeout")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let mut cmd = if use_timeout {
        let mut c = Command::new("timeout");
        c.arg(format!("{}", timeout.as_secs()));
        c.arg(prog);
        c.args(args);
        c
    } else {
        let mut c = Command::new(prog);
        c.args(args);
        c
    };
    let out = cmd.output().context(format!(
        "コマンドの起動に失敗しました: {prog} (NetworkManagerが未導入の可能性があります)"
    ))?;
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&out.stdout));
    if !out.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    if out.status.success() {
        Ok(combined)
    } else {
        let code = out.status.code().map(|c| c.to_string()).unwrap_or_default();
        anyhow::bail!("`{prog} {}` が失敗(終了コード{code}):\n{combined}", args.join(" "))
    }
}

/// import 出力からプロファイル名を推測する。
fn parse_imported_profile(output: &str, fallback: &str) -> String {
    // 例: `Connection 'VG-xxx' (uuid) successfully added.`
    for line in output.lines() {
        let line = line.trim();
        if let Some(start) = line.find('\'') {
            let rest = &line[start + 1..];
            if let Some(end) = rest.find('\'') {
                let name = rest[..end].trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
        if let Some(start) = line.find('"') {
            let rest = &line[start + 1..];
            if let Some(end) = rest.find('"') {
                let name = rest[..end].trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    fallback.to_string()
}

/// 接続の実処理 (ブロッキング。呼び出し側はスレッドで回すこと)。
/// 戻り値: (プロファイル名, ovpnパス, 資格情報パス, ログ)。
pub fn connect_blocking(
    server_hostname: &str,
    config_base64: &str,
    log_fn: &impl Fn(&str),
) -> Result<(String, PathBuf, Option<PathBuf>)> {
    let (ovpn_path, cred_path) = write_ovpn_temp(config_base64, server_hostname)?;
    log_fn(&format!("一時ovpnを作成しました: {}", ovpn_path.display()));

    let want_profile = profile_name_for(server_hostname);
    let ovpn_str = ovpn_path.to_string_lossy().to_string();

    let import_out = run_cmd(
        "nmcli",
        &["connection", "import", "type", "openvpn", "file", &ovpn_str],
        Duration::from_secs(30),
    )
    .map_err(|e| {
        let _ = std::fs::remove_file(&ovpn_path);
        if let Some(c) = &cred_path {
            let _ = std::fs::remove_file(c);
        }
        anyhow::anyhow!("{e:#}\n別サーバーで再試行してください")
    })?;
    log_fn(&format!("nmcli import 完了:\n{import_out}"));
    let profile = parse_imported_profile(&import_out, &want_profile);
    log_fn(&format!("プロファイル名: {profile}"));

    // 認証情報 (失敗しても接続自体は試す。VPN Gate は vpn/vpn が既定)。
    match run_cmd(
        "nmcli",
        &[
            "connection",
            "modify",
            &profile,
            "+vpn.data",
            "username=vpn",
        ],
        Duration::from_secs(15),
    ) {
        Ok(o) => log_fn(&format!("username 設定: {o}")),
        Err(e) => log_fn(&format!("username 設定をスキップ: {e:#}")),
    }
    match run_cmd(
        "nmcli",
        &[
            "connection",
            "modify",
            &profile,
            "+vpn.secrets",
            "password=vpn",
        ],
        Duration::from_secs(15),
    ) {
        Ok(o) => log_fn(&format!("password 設定: {o}")),
        Err(e) => log_fn(&format!("password 設定をスキップ: {e:#}")),
    }

    let up_out = run_cmd(
        "nmcli",
        &["connection", "up", "id", &profile],
        Duration::from_secs(60),
    )
    .map_err(|e| {
        // up 失敗時はプロファイルを消して後片付け
        let _ = run_cmd(
            "nmcli",
            &["connection", "delete", "id", &profile],
            Duration::from_secs(15),
        );
        let _ = std::fs::remove_file(&ovpn_path);
        if let Some(c) = &cred_path {
            let _ = std::fs::remove_file(c);
        }
        anyhow::anyhow!("接続に失敗しました:\n{e:#}\n別サーバーで再試行してください")
    })?;
    log_fn(&format!("nmcli up 完了:\n{up_out}"));

    Ok((profile, ovpn_path, cred_path))
}

/// 指定プロファイルが現在アクティブかを確認する。
fn is_profile_active(profile: &str) -> bool {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME", "connection", "show", "--active"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.trim() == profile),
        _ => false,
    }
}

/// 自アプリ名義 (prefix) の残留プロファイルを掃除する。
/// 終了時の取りこぼし (接続試行中の確定など) を想定し、
/// アクティブなものだけ down + delete する。
pub fn cleanup_stale_profiles(prefix: &str, log_fn: &impl Fn(&str)) {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "NAME", "connection", "show", "--active"])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = line.trim();
        if name.starts_with(prefix) {
            log_fn(&format!("残留プロファイルを切断します: {name}"));
            let _ = run_cmd(
                "nmcli",
                &["connection", "down", "id", name],
                Duration::from_secs(30),
            );
            let _ = run_cmd(
                "nmcli",
                &["connection", "delete", "id", name],
                Duration::from_secs(15),
            );
        }
    }
}

/// 切断の実処理 (ブロッキング)。
/// down を最大3回試し、アクティブ一覧から消えたことを確認してから
/// 後片付けをする。消えなければ Err を返し、呼び出し側で明示する。
pub fn disconnect_blocking(
    profile: &str,
    ovpn_path: Option<&Path>,
    cred_path: Option<&Path>,
    log_fn: &impl Fn(&str),
) -> Result<()> {
    let mut down_ok = false;
    for attempt in 1..=3 {
        match run_cmd(
            "nmcli",
            &["connection", "down", "id", profile],
            Duration::from_secs(30),
        ) {
            Ok(o) => {
                log_fn(&format!("nmcli down 完了:\n{o}"));
                down_ok = true;
                break;
            }
            Err(e) => {
                // 既に非アクティブな場合も成功扱いにする
                if is_profile_active(profile) {
                    log_fn(&format!("nmcli down 再試行 {attempt}/3: {e:#}"));
                    std::thread::sleep(Duration::from_secs(2));
                } else {
                    log_fn("プロファイルは既に非アクティブでした。");
                    down_ok = true;
                    break;
                }
            }
        }
    }
    if !down_ok {
        anyhow::bail!("切断コマンドが3回失敗しました。手動で `nmcli connection down id {profile}` を実行してください");
    }
    // down 直後に残留していないか最終確認する
    std::thread::sleep(Duration::from_secs(1));
    if is_profile_active(profile) {
        anyhow::bail!("{profile} がまだアクティブです。再度 [切断] を押すか、手動で `nmcli connection down id {profile}` を実行してください");
    }
    match run_cmd(
        "nmcli",
        &["connection", "delete", "id", profile],
        Duration::from_secs(15),
    ) {
        Ok(o) => log_fn(&format!("プロファイル削除:\n{o}")),
        Err(e) => log_fn(&format!("プロファイル削除で警告(続行): {e:#}")),
    }
    // 直起動 (F-06b) の残骸 openvpn プロセスがあれば止める
    if let Some(p) = ovpn_path {
        let path_str = p.to_string_lossy().to_string();
        match Command::new("pkill").args(["-f", &path_str]).output() {
            Ok(o) if o.status.success() => log_fn("残留 openvpn プロセスを停止しました。"),
            _ => {}
        }
    }
    if let Some(p) = ovpn_path {
        match std::fs::remove_file(p) {
            Ok(()) => log_fn(&format!("一時ovpnを削除しました: {}", p.display())),
            Err(e) if p.exists() => {
                anyhow::bail!("一時ovpnの削除に失敗しました ({}): {e}", p.display())
            }
            Err(_) => {}
        }
    }
    if let Some(c) = cred_path {
        let _ = std::fs::remove_file(c);
    }
    Ok(())
}
