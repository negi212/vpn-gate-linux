# VPN Gate GUI (Ubuntu用)

Windows版「VPN Gate Clientプラグイン」のように、一覧から選んでワンクリックで
接続できるデスクトップアプリです。Rust + eframe/egui の単一バイナリです。
( Tauri / Electron / Python は使っていません。要件定義.md §8 の候補とは異なり、
軽量な純Rust GUIを採用しています。)

## 動作環境

- Ubuntu 22.04 / 24.04 / 26.04、GNOME (Wayland / X11 どちらも可)
- Rust 1.94 (edition 2021)
- `openvpn`、`network-manager-openvpn (+-gnome)`、`nmcli` が使えること
- 日本語表示には Noto Sans CJK (通常Ubuntuに同梱) を自動利用

## インストールと起動

```bash
sudo apt install openvpn network-manager-openvpn network-manager-openvpn-gnome
cd /home/yuki/vpn-gate
cargo build
cargo run
```

確認コマンド:

```bash
cargo check   # 警告ゼロであること
cargo clippy  # 警告ゼロであること
cargo test    # パーサの単体テスト
```

## 使い方

1. 起動すると自動で `https://www.vpngate.net/api/iphone/` から一覧を取得します
   (15秒タイムアウト。失敗時は15分以内のキャッシュを表示し、理由を通知)。
2. 上部のバーで絞り込みます。
   - 国ドロップダウン (データから動的生成) +「すべて」
   - キーワード検索 (ホスト名 / IP / 運営者)
   - Ping上限スライダー (0 = 無制限)、Score下限、「お気に入りのみ」
   - ソート (Score / 速度 / Ping / 稼働時間 × 昇順/降順)
3. 一覧から行を選んで［接続］(ダブルクリックでも接続)。
   Base64のovpnを `/tmp` に権限0600で書き出し、
   `nmcli connection import` → `nmcli connection up` します。
   認証は既定の `vpn` / `vpn` を自動設定します。
4. ［切断］で `nmcli down` → プロファイル削除 → 一時ovpn削除します。
5. 状態行に 未接続 / 接続中(プロファイル名・経過時間・現IP) / 接続失敗 を表示。
   現IPは `https://ifconfig.me` で裏スレッド取得します。
6. ログ欄に nmcli の出力が時刻付きで追記されます。
   `~/.cache/vpngate-gui/vpngate.log` にも保存されます。

## ファイル構成

- `Cargo.toml` — パッケージ定義 (eframe/egui, reqwest, base64, serde, chrono, anyhow)
- `src/main.rs` — 起動処理
- `src/app.rs` — 画面と状態管理 (F-01〜F-09、非同期は thread + mpsc)
- `src/vpngate.rs` — API取得・CSVパース・キャッシュ (Base64は保持のみ)
- `src/nmcli.rs` — 接続/切断 (0600一時ovpn、auth-user-pass対応)
- `src/store.rs` — お気に入り (`~/.config/vpngate-gui/favorites.json`) とログ

## 注意

VPN Gateは筑波大学の学術実験・ボランティア中継です。
運営者がログを取得する可能性があり、匿名化目的には向きません。
