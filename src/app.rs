//! メイン画面 (F-01〜F-09)。すべてのネットワーク/nmcli処理は
//! バックグラウンドスレッド + mpsc チャネルで行い、UIスレッドを塞がない。

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use crate::nmcli;
use crate::store;
use crate::vpngate::{self, VpnServer};

/// ソートキー (F-04)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Score,
    Speed,
    Ping,
    Uptime,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Score => "Score",
            SortKey::Speed => "速度",
            SortKey::Ping => "Ping",
            SortKey::Uptime => "稼働時間",
        }
    }
}

/// ソートコンボの8項目 (キー×昇降順) を表示順に並べる。
const SORT_OPTIONS: [(SortKey, bool); 8] = [
    (SortKey::Score, false),
    (SortKey::Score, true),
    (SortKey::Speed, false),
    (SortKey::Speed, true),
    (SortKey::Ping, false),
    (SortKey::Ping, true),
    (SortKey::Uptime, false),
    (SortKey::Uptime, true),
];

/// 接続状態 (F-08)。
enum ConnStatus {
    Disconnected,
    Connecting { host: String },
    Connected(ConnectedInfo),
    Failed(String),
}

struct ConnectedInfo {
    profile: String,
    host: String,
    started: Instant,
    ovpn_path: std::path::PathBuf,
    cred_path: Option<std::path::PathBuf>,
    current_ip: Option<String>,
}

/// バックグラウンド→UI への接続イベント。
enum ConnEvent {
    Log(String),
    Done(Result<ConnDone, String>),
}

struct ConnDone {
    profile: String,
    host: String,
    ovpn_path: std::path::PathBuf,
    cred_path: Option<std::path::PathBuf>,
}

/// 取得スレッドの結果 (F-01)。
enum FetchEvent {
    Ok { servers: Vec<VpnServer>, warn: Option<String> },
    Err(String),
}

pub struct VpnGuiApp {
    servers: Vec<VpnServer>,
    fetching: bool,
    fetch_rx: Option<Receiver<FetchEvent>>,
    fetch_error: Option<String>,
    fetch_warn: Option<String>,
    last_updated: Option<String>,

    country: String,
    keyword: String,
    ping_max: i32,
    score_min: i64,
    fav_only: bool,
    favorites: HashSet<String>,

    sort_key: SortKey,
    sort_asc: bool,
    filter_panel_open: bool,
    selected_host: Option<String>,

    status: ConnStatus,
    conn_rx: Option<Receiver<ConnEvent>>,
    ip_rx: Option<Receiver<String>>,
    base_ip: Option<String>,

    logs: Vec<String>,
}

impl VpnGuiApp {
    pub fn new(ctx: &egui::Context) -> Self {
        setup_japanese_fonts(ctx);
        setup_visuals(ctx);
        let favorites = store::load_favorites();
        let mut app = Self {
            servers: Vec::new(),
            fetching: false,
            fetch_rx: None,
            fetch_error: None,
            fetch_warn: None,
            last_updated: None,
            country: "すべて".to_string(),
            keyword: String::new(),
            ping_max: 0,
            score_min: 0,
            fav_only: false,
            favorites,
            sort_key: SortKey::Score,
            sort_asc: false,
            filter_panel_open: true,
            selected_host: None,
            status: ConnStatus::Disconnected,
            conn_rx: None,
            ip_rx: None,
            base_ip: None,
            logs: Vec::new(),
        };
        app.push_log("起動しました。［更新］ボタンでサーバー一覧を取得してください。");
        // キャッシュがあれば即時表示 (F-01)。
        let cache = vpngate::cache_path();
        if cache.exists() {
            match vpngate::load_cache(&cache)
                .and_then(|t| vpngate::parse_api_text(&t)) {
                Ok(servers) if !servers.is_empty() => {
                    app.push_log(&format!(
                        "キャッシュから{}件を読み込みました。",
                        servers.len()
                    ));
                    app.servers = servers;
                    app.last_updated = Some("キャッシュ表示".to_string());
                }
                _ => {}
            }
        }
        // 元IPの取得は裏で (F-08)。
        app.refresh_base_ip();
        // 起動直後に自動取得も試みる (失敗してもキャッシュ表示で継続)。
        app.start_fetch();
        app
    }

    fn push_log(&mut self, msg: &str) {
        let line = store::stamped(msg);
        store::append_log_file(&line);
        self.logs.push(line);
        if self.logs.len() > 2000 {
            let drop_n = self.logs.len() - 2000;
            self.logs.drain(..drop_n);
        }
    }

    // ---- F-01: バックグラウンド取得 ----
    fn start_fetch(&mut self) {
        if self.fetching {
            return;
        }
        self.fetching = true;
        self.fetch_error = None;
        self.fetch_warn = None;
        self.push_log("サーバー一覧を取得しています…");
        let (tx, rx): (Sender<FetchEvent>, Receiver<FetchEvent>) = mpsc::channel();
        self.fetch_rx = Some(rx);
        std::thread::spawn(move || {
            let ev = match vpngate::fetch_with_cache_fallback() {
                Ok((servers, warn)) => FetchEvent::Ok { servers, warn },
                Err(e) => FetchEvent::Err(format!("{e:#}")),
            };
            let _ = tx.send(ev);
        });
    }

    fn poll_fetch(&mut self) {
        let done = if let Some(rx) = &self.fetch_rx {
            match rx.try_recv() {
                Ok(ev) => Some(ev),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(FetchEvent::Err("取得スレッドが中断しました。".to_string()))
                }
            }
        } else {
            None
        };
        if let Some(ev) = done {
            self.fetch_rx = None;
            self.fetching = false;
            match ev {
                FetchEvent::Ok { servers, warn } => {
                    self.push_log(&format!("{}件のサーバーを取得しました。", servers.len()));
                    if let Some(w) = warn {
                        self.push_log(&w);
                        self.fetch_warn = Some(w);
                    }
                    self.servers = servers;
                    self.fetch_error = None;
                    self.last_updated = Some(
                        chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                    );
                }
                FetchEvent::Err(e) => {
                    self.push_log(&format!("取得失敗: {e}"));
                    self.fetch_error = Some(e);
                }
            }
        }
    }

    // ---- F-08: IP表示 ----
    fn refresh_base_ip(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.ip_rx = Some(rx);
        std::thread::spawn(move || {
            if let Ok(ip) = vpngate::fetch_current_ip() {
                let _ = tx.send(ip);
            }
        });
    }

    fn poll_ip(&mut self) {
        if let Some(rx) = &self.ip_rx {
            match rx.try_recv() {
                Ok(ip) => {
                    self.ip_rx = None;
                    let ip = shorten_ip(&ip);
                    // 接続中なら接続先IP、そうでなければ元IPとして保持
                    if let ConnStatus::Connected(info) = &mut self.status {
                        info.current_ip = Some(ip.clone());
                        self.push_log(&format!("現在のIP: {ip}"));
                    } else {
                        self.base_ip = Some(ip);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ip_rx = None;
                }
            }
        }
    }

    // ---- F-06a 接続 / F-07 切断 ----
    fn selected_server(&self) -> Option<VpnServer> {
        let host = self.selected_host.as_ref()?;
        self.servers.iter().find(|s| &s.hostname == host).cloned()
    }

    fn start_connect(&mut self) {
        let Some(server) = self.selected_server() else {
            self.push_log("サーバーが選択されていません。");
            return;
        };
        if matches!(
            self.status,
            ConnStatus::Connecting { .. } | ConnStatus::Connected(_)
        ) {
            self.push_log("既に接続中です。先に切断してください。");
            return;
        }
        let host = server.hostname.clone();
        let b64 = server.config_base64.clone();
        self.status = ConnStatus::Connecting { host: host.clone() };
        self.push_log(&format!("{host} に接続しています…"));
        let (tx, rx): (Sender<ConnEvent>, Receiver<ConnEvent>) = mpsc::channel();
        self.conn_rx = Some(rx);
        std::thread::spawn(move || {
            let log_tx = tx.clone();
            let log_fn = |msg: &str| {
                let _ = log_tx.send(ConnEvent::Log(msg.to_string()));
            };
            match nmcli::connect_blocking(&host, &b64, &log_fn) {
                Ok((profile, ovpn_path, cred_path)) => {
                    let _ = tx.send(ConnEvent::Done(Ok(ConnDone {
                        profile,
                        host,
                        ovpn_path,
                        cred_path,
                    })));
                }
                Err(e) => {
                    let _ = tx.send(ConnEvent::Done(Err(format!("{e:#}"))));
                }
            }
        });
    }

    fn start_disconnect(&mut self) {
        let (profile, ovpn_path, cred_path, host) =
            match std::mem::replace(&mut self.status, ConnStatus::Disconnected) {
                ConnStatus::Connected(info) => (
                    info.profile,
                    info.ovpn_path,
                    info.cred_path,
                    info.host,
                ),
                ConnStatus::Connecting { host } => {
                    // 接続試行中のキャンセル: 状態だけ戻す
                    self.push_log(&format!("{host} への接続試行を中止しました。"));
                    self.status = ConnStatus::Disconnected;
                    return;
                }
                other => {
                    self.status = other;
                    self.push_log("接続していません。");
                    return;
                }
            };
        self.status = ConnStatus::Disconnected;
        self.push_log(&format!("{profile} から切断しています…"));
        let _ = host;
        let (tx, rx): (Sender<ConnEvent>, Receiver<ConnEvent>) = mpsc::channel();
        self.conn_rx = Some(rx);
        std::thread::spawn(move || {
            let log_tx = tx.clone();
            let log_fn = |msg: &str| {
                let _ = log_tx.send(ConnEvent::Log(msg.to_string()));
            };
            match nmcli::disconnect_blocking(
                &profile,
                Some(&ovpn_path),
                cred_path.as_deref(),
                &log_fn,
            ) {
                Ok(()) => {
                    let _ = tx.send(ConnEvent::Log("切断しました。".to_string()));
                }
                Err(e) => {
                    let _ = tx.send(ConnEvent::Log(format!("切断に失敗しました: {e:#}")));
                }
            }
        });
        // 切断後は元IP表示に戻すため再取得
        self.refresh_base_ip();
    }

    fn poll_conn(&mut self) {
        let mut done: Option<Result<ConnDone, String>> = None;
        // レシーバを一旦取り出して所有することで借用競合を避ける。
        if let Some(rx) = self.conn_rx.take() {
            let mut keep = false;
            loop {
                match rx.try_recv() {
                    Ok(ConnEvent::Log(msg)) => self.push_log(&msg),
                    Ok(ConnEvent::Done(r)) => {
                        done = Some(r);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        keep = true;
                        break;
                    }
                    // 送信側(スレッド)終了。キューに残ったLogは上で回収済み。
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            if keep && done.is_none() {
                self.conn_rx = Some(rx);
            }
        }
        match done {
            Some(Ok(d)) => {
                self.conn_rx = None;
                self.push_log(&format!(
                    "接続しました: {} (プロファイル {})",
                    d.host, d.profile
                ));
                self.status = ConnStatus::Connected(ConnectedInfo {
                    profile: d.profile,
                    host: d.host,
                    started: Instant::now(),
                    ovpn_path: d.ovpn_path,
                    cred_path: d.cred_path,
                    current_ip: None,
                });
                // 接続後IPを取得 (F-08)
                let (tx, rx) = mpsc::channel();
                self.ip_rx = Some(rx);
                std::thread::spawn(move || {
                    if let Ok(ip) = vpngate::fetch_current_ip() {
                        let _ = tx.send(ip);
                    }
                });
            }
            Some(Err(e)) => {
                self.conn_rx = None;
                self.push_log(&format!("接続失敗: {e}"));
                self.status = ConnStatus::Failed(e);
            }
            None => {}
        }
    }

    // ---- F-03/F-04: フィルタ・ソート ----
    fn filtered_indices(&self) -> Vec<usize> {
        let kw = self.keyword.trim().to_lowercase();
        let mut idx: Vec<usize> = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if self.country != "すべて" && s.country_short != self.country {
                    return false;
                }
                if self.fav_only && !self.favorites.contains(&s.hostname) {
                    return false;
                }
                if self.ping_max > 0 && (s.ping_ms < 0 || s.ping_ms > self.ping_max as i64) {
                    return false;
                }
                if s.score < self.score_min {
                    return false;
                }
                if !kw.is_empty() {
                    let hay = format!("{} {} {}", s.hostname, s.ip, s.operator).to_lowercase();
                    if !hay.contains(&kw) {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect();
        idx.sort_by(|&a, &b| {
            let sa = &self.servers[a];
            let sb = &self.servers[b];
            let ord = match self.sort_key {
                SortKey::Score => sa.score.cmp(&sb.score),
                SortKey::Speed => sa.speed_bps.cmp(&sb.speed_bps),
                SortKey::Ping => sa.ping_ms.cmp(&sb.ping_ms),
                SortKey::Uptime => sa.uptime_secs.cmp(&sb.uptime_secs),
            };
            if self.sort_asc { ord } else { ord.reverse() }
        });
        idx
    }

    fn toggle_favorite(&mut self) {
        let Some(server) = self.selected_server() else {
            self.push_log("サーバーが選択されていません。");
            return;
        };
        if self.favorites.remove(&server.hostname) {
            self.push_log(&format!("お気に入りから削除: {}", server.hostname));
        } else {
            self.favorites.insert(server.hostname.clone());
            self.push_log(&format!("お気に入りに追加: {}", server.hostname));
        }
        if let Err(e) = store::save_favorites(&self.favorites) {
            self.push_log(&format!("お気に入り保存に失敗: {e:#}"));
        }
    }
}

impl eframe::App for VpnGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_fetch();
        self.poll_conn();
        self.poll_ip();
        // 接続中は経過時間表示のため再描画
        if matches!(
            self.status,
            ConnStatus::Connecting { .. } | ConnStatus::Connected(_)
        ) {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }

        // 国一覧はデータから動的生成 (F-03)
        let mut countries: BTreeMap<String, String> = BTreeMap::new();
        for s in &self.servers {
            countries
                .entry(s.country_short.clone())
                .or_insert_with(|| s.country_long.clone());
        }

        let filtered = self.filtered_indices();
        let selected_host = self.selected_host.clone();

        let style = ctx.style();

        egui::TopBottomPanel::top("toolbar")
            .frame(glass_frame(
                &style,
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 16,
                    se: 16,
                },
                egui::Margin::symmetric(18, 9),
            ))
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("VPN Gate")
                        .size(18.0)
                        .strong()
                        .color(INDIGO_SOFT),
                );
                ui.add_space(4.0);
                ui.separator();
                if ui
                    .add_enabled(
                        !self.fetching,
                        accent_button("更新", Accent::Indigo),
                    )
                    .clicked()
                {
                    self.start_fetch();
                }
                if self.fetching {
                    ui.spinner();
                    ui.label(egui::RichText::new("取得中…").color(MUTED));
                }
                ui.add_space(4.0);
                ui.separator();
                if ui
                    .selectable_label(self.filter_panel_open, "絞り込み")
                    .clicked()
                {
                    self.filter_panel_open = !self.filter_panel_open;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}件 / 全{}件",
                            filtered.len(),
                            self.servers.len()
                        ))
                        .monospace()
                        .size(13.0)
                        .color(EMERALD_SOFT),
                    );
                });
            });
        });

        if self.filter_panel_open {
            egui::SidePanel::left("filter_panel")
                .resizable(true)
                .default_width(250.0)
                .min_width(200.0)
                .frame(glass_frame(
                    &style,
                    egui::CornerRadius {
                        nw: 0,
                        ne: 16,
                        sw: 0,
                        se: 16,
                    },
                    egui::Margin::symmetric(16, 12),
                ))
                .show(ctx, |ui| {
                ui.label(egui::RichText::new("絞り込み").size(20.0).strong());
                ui.add_space(6.0);
                ui.separator();

                section_label(ui, "国");
                egui::ComboBox::from_id_salt("country")
                    .selected_text(&self.country)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.country, "すべて".to_string(), "すべて");
                        for (short, long) in &countries {
                            ui.selectable_value(
                                &mut self.country,
                                short.clone(),
                                format!("{short} {long}"),
                            );
                        }
                    });

                section_label(ui, "検索");
                let w = ui.available_width();
                ui.add(
                    egui::TextEdit::singleline(&mut self.keyword)
                        .hint_text("ホスト名 / IP / 運営者")
                        .desired_width(w),
                );

                section_label(ui, "ソート");
                egui::ComboBox::from_id_salt("sort_combo")
                    .selected_text(format!(
                        "{}{}",
                        self.sort_key.label(),
                        if self.sort_asc { "昇順" } else { "降順" }
                    ))
                    .show_ui(ui, |ui| {
                        for (key, asc) in SORT_OPTIONS {
                            let is_cur = self.sort_key == key && self.sort_asc == asc;
                            let text = format!(
                                "{}{}",
                                key.label(),
                                if asc { "昇順" } else { "降順" }
                            );
                            if ui.selectable_label(is_cur, text).clicked() {
                                self.sort_key = key;
                                self.sort_asc = asc;
                            }
                        }
                    });

                section_label(ui, if self.ping_max == 0 {
                    "Ping上限: 無制限"
                } else {
                    "Ping上限 (ms):"
                });
                ui.add(egui::Slider::new(&mut self.ping_max, 0..=500).text(""));

                section_label(ui, "Score下限");
                ui.add(egui::DragValue::new(&mut self.score_min).range(0..=i64::MAX).speed(1000));

                ui.add_space(10.0);
                ui.checkbox(&mut self.fav_only, "お気に入りのみ");

                ui.add_space(10.0);
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{}件 / 全{}件", filtered.len(), self.servers.len()))
                        .monospace()
                        .size(13.0)
                        .color(EMERALD_SOFT),
                );
                if let Some(t) = &self.last_updated {
                    ui.label(egui::RichText::new(format!("最終更新: {t}")).small().color(MUTED));
                }
                if let Some(e) = self.fetch_error.clone() {
                    ui.colored_label(ROSE, format!("取得エラー: {e}"));
                }
                if let Some(w) = self.fetch_warn.clone() {
                    ui.colored_label(AMBER, w);
                }
            });
        }

        egui::TopBottomPanel::bottom("statusbar")
            .frame(glass_frame(
                &style,
                egui::CornerRadius {
                    nw: 16,
                    ne: 16,
                    sw: 0,
                    se: 0,
                },
                egui::Margin::symmetric(18, 10),
            ))
            .show(ctx, |ui| {
            let (conn_label, conn_enabled) = match &self.status {
                ConnStatus::Disconnected => ("接続", true),
                ConnStatus::Connecting { .. } => ("接続中…", false),
                ConnStatus::Connected(_) => ("切断", true),
                ConnStatus::Failed(_) => ("再接続", true),
            };
            let status_line: (String, egui::Color32) = match &self.status {
                ConnStatus::Disconnected => {
                    let raw = self.base_ip.as_deref().unwrap_or("取得中…");
                    let ip = if raw == "取得中…" {
                        raw.to_string()
                    } else {
                        shorten_ip(raw)
                    };
                    (format!("状態: 未接続 (現IP: {ip})"), MUTED)
                }
                ConnStatus::Connecting { host } => {
                    (format!("状態: 接続中… ({host})"), AMBER)
                }
                ConnStatus::Connected(info) => {
                    let el = info.started.elapsed().as_secs();
                    let raw = info.current_ip.as_deref().unwrap_or("取得中…");
                    let ip = if raw == "取得中…" {
                        raw.to_string()
                    } else {
                        shorten_ip(raw)
                    };
                    (
                        format!(
                            "状態: 接続中 ({} / {} / {:02}:{:02}:{:02}経過 / 現IP: {})",
                            info.host,
                            info.profile,
                            el / 3600,
                            (el % 3600) / 60,
                            el % 60,
                            ip
                        ),
                        EMERALD,
                    )
                }
                ConnStatus::Failed(e) => (
                    format!("状態: 接続失敗 — {e} 別サーバーで再試行してください"),
                    ROSE,
                ),
            };
            let fav_label = match &selected_host {
                Some(h) if self.favorites.contains(h) => "★ お気に入り解除",
                _ => "☆ お気に入り追加",
            };
            let conn_accent = if matches!(self.status, ConnStatus::Connected(_)) {
                Accent::Emerald
            } else {
                Accent::Indigo
            };

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(conn_enabled, accent_button(conn_label, conn_accent))
                    .clicked()
                {
                    if matches!(self.status, ConnStatus::Connected(_)) {
                        self.start_disconnect();
                    } else {
                        self.start_connect();
                    }
                }
                if ui
                    .add(egui::Button::new(fav_label).corner_radius(10))
                    .clicked()
                {
                    self.toggle_favorite();
                }
                let (text, color) = status_line;
                ui.add_space(4.0);
                ui.colored_label(color, text);
            });
            ui.add_space(4.0);
            section_label(ui, "状態");
            egui::ScrollArea::vertical()
                .max_height(100.0)
                .auto_shrink([false, true])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.logs.join("\n"))
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .font(egui::TextStyle::Monospace),
                    );
                });
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "VPN Gateは筑波大学の学術実験・ボランティア中継です。運営者がログを取得する可能性があり、匿名化目的には向きません。",
                )
                .small()
                .color(MUTED),
            );
        });

        // F-02: 中央テーブル (仮想化: 可視行のみ描画)
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&style)
                    .inner_margin(egui::Margin::symmetric(20, 16)),
            )
            .show(ctx, |ui| {
            let mut title = egui::text::LayoutJob::default();
            title.append(
                "サーバー一覧 (",
                0.0,
                heading_format(INK),
            );
            title.append(
                &filtered.len().to_string(),
                0.0,
                heading_num_format(EMERALD_SOFT),
            );
            title.append("件)", 0.0, heading_format(INK));
            ui.add(egui::Label::new(title).selectable(false));
            ui.add_space(8.0);
            egui::ScrollArea::horizontal().show(ui, |ui| {
                // ヘッダ
                ui.horizontal(|ui| {
                    let head = |s: String| {
                        egui::RichText::new(s).monospace().size(13.0).color(MUTED)
                    };
                    ui.label(head("★ ".to_string()));
                    ui.label(head(format!("{:<8}", "国")));
                    ui.label(head(format!("{:<28}", "ホスト名")));
                    ui.label(head(format!("{:<15}", "IP")));
                    ui.label(head(format!("{:>8}", "Ping")));
                    ui.label(head(format!("{:>10}", "速度")));
                    ui.label(head(format!("{:>10}", "Score")));
                    ui.label(head(format!("{:<10}", "稼働")));
                    ui.label(head(format!("{:>6}", "混雑")));
                    ui.label(head("運営者".to_string()));
                });
                ui.separator();
                ui.add_space(4.0);
                let row_h = 28.0;
                // 行データを事前に所有型で作り、描画クロージャが &mut self を
                // 触らずに済むようにする。クリックは後で適用する。
                struct RowData {
                    hostname: String,
                    country_short: String,
                    ip: String,
                    ping_ms: i64,
                    speed: String,
                    score: i64,
                    uptime: String,
                    sessions: i64,
                    operator: String,
                    is_sel: bool,
                    fav_mark: &'static str,
                }
                let rows: Vec<RowData> = filtered
                    .iter()
                    .map(|&si| {
                        let s = &self.servers[si];
                        RowData {
                            hostname: s.hostname.clone(),
                            country_short: s.country_short.clone(),
                            ip: s.ip.clone(),
                            ping_ms: s.ping_ms,
                            speed: s.speed_mbps_display().replace(" Mbps", "M"),
                            score: s.score,
                            uptime: s.uptime_display(),
                            sessions: s.num_sessions,
                            operator: truncate(&s.operator, 20),
                            is_sel: selected_host.as_deref() == Some(s.hostname.as_str()),
                            fav_mark: if self.favorites.contains(&s.hostname) {
                                "★"
                            } else {
                                "☆"
                            },
                        }
                    })
                    .collect();
                let mut clicked: Option<String> = None;
                let mut dbl_clicked: Option<String> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, rows.len(), |ui, range| {
                        for i in range {
                            let r = &rows[i];
                            let ping = if r.ping_ms < 0 {
                                "-".to_string()
                            } else {
                                r.ping_ms.to_string()
                            };
                            let mut job = egui::text::LayoutJob::default();
                            job.append(
                                &format!(
                                    "{:<8} {:<28} {:<15}",
                                    r.country_short,
                                    truncate(&r.hostname, 28),
                                    r.ip
                                ),
                                0.0,
                                row_format(ROW_TEXT),
                            );
                            job.append(
                                &format!(" {:>7}ms", ping),
                                0.0,
                                row_format(INDIGO_SOFT),
                            );
                            job.append(&format!(" {:>10}", r.speed), 0.0, row_format(EMERALD_SOFT));
                            job.append(&format!(" {:>10}", r.score), 0.0, row_format(INDIGO_SOFT));
                            job.append(
                                &format!(" {:<10} {:>6}", r.uptime, r.sessions),
                                0.0,
                                row_format(MUTED),
                            );
                            job.append(&format!(" {}", r.operator), 0.0, row_format(MUTED));
                            ui.horizontal(|ui| {
                                let star_color = if r.fav_mark == "★" { AMBER } else { FAINT };
                                ui.label(
                                    egui::RichText::new(r.fav_mark)
                                        .monospace()
                                        .size(13.0)
                                        .color(star_color),
                                );
                                let resp = ui.selectable_label(r.is_sel, job);
                                if resp.clicked() {
                                    clicked = Some(r.hostname.clone());
                                }
                                if resp.double_clicked() {
                                    dbl_clicked = Some(r.hostname.clone());
                                }
                            });
                        }
                    });
                if let Some(h) = dbl_clicked {
                    self.selected_host = Some(h);
                    self.start_connect();
                } else if let Some(h) = clicked {
                    self.selected_host = Some(h);
                }
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let ConnStatus::Connected(info) =
            std::mem::replace(&mut self.status, ConnStatus::Disconnected)
        {
            eprintln!("vpngate-gui: 終了時切断 {}", info.profile);
            let log_fn = |msg: &str| eprintln!("vpngate-gui: {msg}");
            if let Err(e) = nmcli::disconnect_blocking(
                &info.profile,
                Some(&info.ovpn_path),
                info.cred_path.as_deref(),
                &log_fn,
            ) {
                eprintln!("vpngate-gui: 終了時切断に失敗: {e:#}");
            }
        }
        nmcli::cleanup_stale_profiles("VG-", &|msg: &str| {
            eprintln!("vpngate-gui: {msg}")
        });
    }
}

// ---- ビジュアルトークン ----
const NAVY_BASE: egui::Color32 = egui::Color32::from_rgb(15, 23, 42); // #0F172A
const DEEP: egui::Color32 = egui::Color32::from_rgb(11, 17, 32); // #0B1120
const INK: egui::Color32 = egui::Color32::from_rgb(226, 232, 240); // #E2E8F0
const ROW_TEXT: egui::Color32 = egui::Color32::from_rgb(203, 213, 225); // #CBD5E1
const MUTED: egui::Color32 = egui::Color32::from_rgb(148, 163, 184); // #94A3B8
const FAINT: egui::Color32 = egui::Color32::from_rgb(100, 116, 139); // #64748B
const INDIGO_SOFT: egui::Color32 = egui::Color32::from_rgb(165, 180, 252); // #A5B4FC
const EMERALD: egui::Color32 = egui::Color32::from_rgb(52, 211, 153); // #34D399
const EMERALD_SOFT: egui::Color32 = egui::Color32::from_rgb(110, 231, 183); // #6EE7B7
const AMBER: egui::Color32 = egui::Color32::from_rgb(251, 191, 36); // #FBBF24
const ROSE: egui::Color32 = egui::Color32::from_rgb(248, 113, 113); // #F87171
fn hairline() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(148, 163, 184, 38)
}

#[derive(Clone, Copy)]
enum Accent {
    Indigo,
    Emerald,
}

fn accent_button(label: &str, accent: Accent) -> egui::Button<'static> {
    let (fill, stroke) = match accent {
        Accent::Indigo => (
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 44),
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 120),
        ),
        Accent::Emerald => (
            egui::Color32::from_rgba_unmultiplied(52, 211, 153, 40),
            egui::Color32::from_rgba_unmultiplied(52, 211, 153, 115),
        ),
    };
    egui::Button::new(egui::RichText::new(label).strong())
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(11)
}

fn glass_frame(
    style: &egui::Style,
    corners: egui::CornerRadius,
    inner: egui::Margin,
) -> egui::Frame {
    egui::Frame::side_top_panel(style)
        .fill(egui::Color32::from_rgba_unmultiplied(30, 41, 59, 190))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 26),
        ))
        .corner_radius(corners)
        .inner_margin(inner)
        .shadow(egui::Shadow {
            offset: [0, 2],
            blur: 18,
            spread: 0,
            color: egui::Color32::from_rgba_unmultiplied(2, 6, 23, 90),
        })
}

fn row_format(color: egui::Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::monospace(13.0),
        color,
        ..Default::default()
    }
}

fn heading_format(color: egui::Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::proportional(21.0),
        color,
        ..Default::default()
    }
}

fn heading_num_format(color: egui::Color32) -> egui::TextFormat {
    egui::TextFormat {
        font_id: egui::FontId::monospace(19.0),
        color,
        ..Default::default()
    }
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::proportional(12.0),
            color: INDIGO_SOFT,
            extra_letter_spacing: 1.6,
            ..Default::default()
        },
    );
    ui.add(egui::Label::new(job).selectable(false));
    ui.add_space(2.0);
}

fn setup_visuals(ctx: &egui::Context) {
    ctx.style_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = NAVY_BASE;
        v.window_fill = egui::Color32::from_rgb(21, 30, 53);
        v.extreme_bg_color = DEEP;
        v.faint_bg_color = egui::Color32::from_rgba_unmultiplied(148, 163, 184, 12);
        v.override_text_color = Some(INK);
        v.weak_text_color = Some(MUTED);
        v.hyperlink_color = INDIGO_SOFT;
        v.disabled_alpha = 0.45;
        v.window_corner_radius = egui::CornerRadius::same(14);
        v.menu_corner_radius = egui::CornerRadius::same(12);
        v.text_edit_bg_color = Some(egui::Color32::from_rgba_unmultiplied(11, 17, 32, 230));
        v.window_shadow = egui::Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: egui::Color32::from_rgba_unmultiplied(2, 6, 23, 120),
        };
        v.popup_shadow = v.window_shadow;

        v.widgets.noninteractive.bg_fill = NAVY_BASE;
        v.widgets.noninteractive.weak_bg_fill = NAVY_BASE;
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, hairline());
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, INK);
        v.widgets.noninteractive.corner_radius = egui::CornerRadius::same(10);

        v.widgets.inactive.bg_fill = egui::Color32::from_rgba_unmultiplied(148, 163, 184, 18);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(148, 163, 184, 14);
        v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, hairline());
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, INK);
        v.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
        v.widgets.inactive.expansion = 1.0;

        v.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(129, 140, 248, 46);
        v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(129, 140, 248, 38);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 110),
        );
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(241, 245, 249));
        v.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
        v.widgets.hovered.expansion = 1.0;

        v.widgets.active.bg_fill = egui::Color32::from_rgba_unmultiplied(99, 102, 241, 96);
        v.widgets.active.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(99, 102, 241, 90);
        v.widgets.active.bg_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 150),
        );
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, INDIGO_SOFT);
        v.widgets.active.corner_radius = egui::CornerRadius::same(10);
        v.widgets.active.expansion = -1.0;

        v.widgets.open.bg_fill = egui::Color32::from_rgba_unmultiplied(129, 140, 248, 56);
        v.widgets.open.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(129, 140, 248, 48);
        v.widgets.open.bg_stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(129, 140, 248, 130),
        );
        v.widgets.open.fg_stroke = egui::Stroke::new(1.0, INDIGO_SOFT);
        v.widgets.open.corner_radius = egui::CornerRadius::same(10);

        v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(52, 211, 153, 64);
        v.selection.stroke = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(52, 211, 153, 170),
        );

        style.spacing.item_spacing = egui::vec2(10.0, 7.0);
        style.spacing.button_padding = egui::vec2(14.0, 6.0);
        style.spacing.interact_size = egui::vec2(24.0, 26.0);
        style.spacing.indent = 14.0;
        style.spacing.slider_width = 180.0;
        style.spacing.scroll.floating = true;
        style.spacing.scroll.bar_width = 8.0;

        style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
        style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(21.0));
    });
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        format!("{s:<width$}", width = max_chars)
    } else {
        chars[..max_chars].iter().collect()
    }
}

fn shorten_ip(s: &str) -> String {
    let one_line: String = s.lines().next().unwrap_or("").trim().to_string();
    if one_line.contains('<') || one_line.contains('>') {
        return "取得失敗".to_string();
    }
    const MAX: usize = 45;
    if one_line.chars().count() > MAX {
        one_line.chars().take(MAX).collect::<String>() + "…"
    } else if one_line.is_empty() {
        "取得失敗".to_string()
    } else {
        one_line
    }
}

/// 日本語フォント設定: システムの Noto CJK / Noto Sans JP があれば読む。
/// 無ければ既定フォントのまま (英語は表示できる)。
fn setup_japanese_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansJP-Regular.otf",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansJP-Regular.otf",
        "/usr/share/fonts/NotoSansCJK-Regular.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(data) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "noto_jp".to_owned(),
                egui::FontData::from_owned(data).into(),
            );
            for family in [
                egui::FontFamily::Proportional,
                egui::FontFamily::Monospace,
            ] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "noto_jp".to_owned());
            }
            ctx.set_fonts(fonts);
            break;
        }
    }
}
