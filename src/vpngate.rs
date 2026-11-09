//! VPN Gate API の取得・パース・キャッシュ (F-01)。
//!
//! Endpoint: `https://www.vpngate.net/api/iphone/` (GET, CSV, UTF-8)
//! - 1行目 `*` 始まりはコメント→無視
//! - 2行目はヘッダ `#HostName,...`
//! - 3行目以降がサーバー行。最終列 `OpenVPN_ConfigData_Base64` が空の行は除外。
//! - Base64 のデコードは接続時(F-06)のみ行う。ここでは文字列のまま保持する。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

pub const API_URL: &str = "https://www.vpngate.net/api/iphone/";
pub const FETCH_TIMEOUT_SECS: u64 = 15;
pub const CACHE_VALID_SECS: u64 = 15 * 60;

/// サーバー1行分。`config_base64` は接続時までデコードしない。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct VpnServer {
    pub hostname: String,
    pub ip: String,
    pub score: i64,
    pub ping_ms: i64,
    pub speed_bps: i64,
    pub country_long: String,
    pub country_short: String,
    pub num_sessions: i64,
    pub uptime_secs: i64,
    pub total_users: i64,
    pub total_traffic: i64,
    pub log_type: String,
    pub operator: String,
    pub message: String,
    pub config_base64: String,
}

impl VpnServer {
    /// 速度表示 (Mbps、日本表記)。
    pub fn speed_mbps_display(&self) -> String {
        format!("{:.1} Mbps", self.speed_bps as f64 / 1_000_000.0)
    }

    /// 稼働時間表示 (X日Y時間)。
    pub fn uptime_display(&self) -> String {
        let days = self.uptime_secs / 86_400;
        let hours = (self.uptime_secs % 86_400) / 3600;
        if days > 0 {
            format!("{days}日{hours}時間")
        } else {
            format!("{hours}時間")
        }
    }
}

/// キャッシュファイルのパス (`~/.cache/vpngate-gui/vpngate_cache.csv`)。
pub fn cache_path() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.cache",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        )
    });
    PathBuf::from(base).join("vpngate-gui").join("vpngate_cache.csv")
}

/// キャッシュが有効期限内(15分)か。
pub fn cache_is_fresh(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    mtime
        .elapsed()
        .map(|d| d.as_secs() < CACHE_VALID_SECS)
        .unwrap_or(false)
}

/// API本文(CSVテキスト)をパースする。Base64はデコードしない。
pub fn parse_api_text(text: &str) -> Result<Vec<VpnServer>> {
    let mut servers = Vec::new();
    let mut lines = text.lines();

    // 1行目: `*` コメント行は無視(無くても許容)。
    let first = lines.next().unwrap_or("");
    let mut header_seen = false;
    let mut data_lines: Vec<&str> = Vec::new();

    // 2行目がヘッダ(`#HostName,...`)のはず。1行目が無く2行目から始まる場合も吸収。
    let mut rest = vec![first];
    rest.extend(lines);
    for line in rest {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line.starts_with('*') {
            continue;
        }
        if !header_seen && line.starts_with('#') {
            header_seen = true;
            continue;
        }
        data_lines.push(line);
    }

    for line in data_lines {
        // 最終列(Base64)にカンマは含まれない前提で後ろから15列分割。
        // ホスト名等にカンマが混じる可能性に備え、素朴な split(',') は使わない。
        let c: Vec<&str> = rsplit_n(line, ',', 15);
        if c.len() != 15 {
            continue;
        }
        let config_base64 = c[14].trim().to_string();
        if config_base64.is_empty() {
            // OpenVPN非対応→除外
            continue;
        }
        servers.push(VpnServer {
            hostname: c[0].trim().to_string(),
            ip: c[1].trim().to_string(),
            score: c[2].trim().parse().unwrap_or(0),
            ping_ms: c[3].trim().parse().unwrap_or(-1),
            speed_bps: c[4].trim().parse().unwrap_or(0),
            country_long: c[5].trim().to_string(),
            country_short: c[6].trim().to_string(),
            num_sessions: c[7].trim().parse().unwrap_or(0),
            uptime_secs: c[8].trim().parse().unwrap_or(0),
            total_users: c[9].trim().parse().unwrap_or(0),
            total_traffic: c[10].trim().parse().unwrap_or(0),
            log_type: c[11].trim().to_string(),
            operator: c[12].trim().to_string(),
            message: c[13].trim().to_string(),
            config_base64,
        });
    }
    Ok(servers)
}

/// 後ろから最大 n 分割する。末尾(n-1)列を正確に切り出し、
/// 先頭の余り(カンマ混じり対策)は先頭要素にまとめる。
fn rsplit_n(s: &str, delim: char, n: usize) -> Vec<&str> {
    let mut parts: Vec<&str> = s.rsplitn(n, delim).collect();
    parts.reverse();
    parts
}

/// ネットワークから取得する。15秒タイムアウト。失敗時は Err。
pub fn fetch_from_network() -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .user_agent("vpngate-gui/0.1")
        .build()
        .context("HTTPクライアントの作成に失敗")?;
    let resp = client
        .get(API_URL)
        .send()
        .context("APIへの接続に失敗しました")?;
    if !resp.status().is_success() {
        anyhow::bail!("API応答エラー: {}", resp.status());
    }
    resp.text().context("API応答の読み取りに失敗しました")
}

/// キャッシュに保存する (親ディレクトリも作成)。
pub fn save_cache(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("キャッシュ目录の作成に失敗")?;
    }
    std::fs::write(path, text).context("キャッシュの保存に失敗")?;
    Ok(())
}

/// キャッシュを読み込む。
pub fn load_cache(path: &std::path::Path) -> Result<String> {
    std::fs::read_to_string(path).context("キャッシュの読み取りに失敗")
}

/// 取得フロー: ネットワーク→成功なら保存してパース。
/// 失敗時は新鮮なキャッシュがあればそれをパースして (servers, 警告) を返す。
pub fn fetch_with_cache_fallback() -> Result<(Vec<VpnServer>, Option<String>)> {
    let path = cache_path();
    match fetch_from_network() {
        Ok(text) => {
            let servers = parse_api_text(&text)?;
            if servers.is_empty() {
                // 空振り: キャッシュがあれば使う
                if let Ok(cached) = load_cache(&path) {
                    let servers = parse_api_text(&cached)?;
                    return Ok((
                        servers,
                        Some(
                            "API応答が空だったためキャッシュを表示しています。"
                                .to_string(),
                        ),
                    ));
                }
                anyhow::bail!("APIからサーバー情報を取得できませんでした(空応答)。");
            }
            let _ = save_cache(&path, &text);
            Ok((servers, None))
        }
        Err(e) => {
            if path.exists() {
                if let Ok(cached) = load_cache(&path) {
                    if let Ok(servers) = parse_api_text(&cached) {
                        if !servers.is_empty() {
                            let stale = if cache_is_fresh(&path) {
                                ""
                            } else {
                                "(15分以上前の古いキャッシュ) "
                            };
                            return Ok((
                                servers,
                                Some(format!(
                                    "ネットワーク取得に失敗したためキャッシュを表示しています{stale}: {e:#}"
                                )),
                            ));
                        }
                    }
                }
            }
            Err(e)
        }
    }
}

/// 現在のグローバルIPを取得する (10秒タイムアウト、F-08用)。
/// text/plain を返す取得先を優先し、HTML応答は拒否して次の候補を試す。
/// 戻り値は必ず1行のIP文字列 (IPv4/IPv6としてパース可能なもののみ)。
pub fn sanitize_ip_text(raw: &str) -> Option<String> {
    let first = raw.lines().next().unwrap_or("").trim();
    if first.is_empty() || first.len() > 64 || first.contains('<') || first.contains('>') {
        return None;
    }
    // IPとしてパースできなければHTMLやエラーページとみなす
    if first.parse::<std::net::IpAddr>().is_ok() {
        Some(first.to_string())
    } else {
        None
    }
}

/// 現在のグローバルIPを取得する (10秒タイムアウト、F-08用)。
pub fn fetch_current_ip() -> Result<String> {
    const SOURCES: &[&str] = &[
        "https://api.ipify.org",
        "https://icanhazip.com",
        "https://ifconfig.me/ip",
    ];
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("vpngate-gui/0.1")
        .build()
        .context("HTTPクライアントの作成に失敗")?;
    let mut last_err: Option<anyhow::Error> = None;
    for url in SOURCES {
        match client
            .get(*url)
            .header("Accept", "text/plain")
            .send()
            .context("IP取得に失敗")
            .and_then(|r| r.text().context("IP応答の読み取りに失敗"))
        {
            Ok(text) => {
                if let Some(ip) = sanitize_ip_text(&text) {
                    return Ok(ip);
                }
                last_err = Some(anyhow::anyhow!("{url} の応答がIP形式ではありません"));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("IP取得に失敗")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_ip_rejects_html() {
        assert_eq!(sanitize_ip_text("1.2.3.4\n"), Some("1.2.3.4".to_string()));
        assert_eq!(sanitize_ip_text("<!DOCTYPE html>\n<html>"), None);
        assert_eq!(sanitize_ip_text("not-an-ip"), None);
    }

    #[test]
    fn parses_rows_and_skips_empty_config() {
        let text = "* comment\n#HostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,OpenVPN_ConfigData_Base64\npublic-a,1.2.3.4,100,10,100000000,Japan,JP,5,90000,10,100,log,op,msg,QUJD\npublic-b,5.6.7.8,50,20,50000000,Japan,JP,3,3600,5,50,log,op2,msg2,\n";
        let servers = parse_api_text(text).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].hostname, "public-a");
        assert_eq!(servers[0].speed_mbps_display(), "100.0 Mbps");
        assert_eq!(servers[0].uptime_display(), "1日1時間");
    }
}
