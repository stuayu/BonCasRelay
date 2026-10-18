# 設計と Web API

## APDU 判定とルーティング

自動判定では APDU を分類し、未確定リーダーに対するプローブ結果をキャッシュして B-CAS / ACAS の配送先を決めます。判定中のリーダーは排他ロックで保護します。カード通信中の設定更新は同一リーダーの処理と直列化し、既存 TCP クライアントは維持します。固定ポートは指定リーダーへ直接配送し、自動ポートと重複する設定は拒否します.

状態 API は現在の reader list、判定、routes、clients、ログをスナップショットとして返します。カード通信のワイヤー形式、判定キャッシュの意味、再判定の動作を変えずに crate 名と UI 表示を整理しています。

## Web API

既定待受は `127.0.0.1:6980`。JSON 応答は UTF-8。Web API には認証もアクセス元の検査もありません。カード通信も無認証です。ファイアウォールと `Allow` でアクセス元を管理する前提です。非ループバック待受では起動時に警告を記録します。CORS は有効にしません。

| Method / path | 内容 |
| --- | --- |
| `GET /api/state` | 状態スナップショット、設定、待受、バージョン |
| `GET /api/config` | 設定、CLI 上書き、保存先、次回起動時の説明 |
| `PUT /api/config` | JSON 設定を検証し、リスナーと保存ファイルへ適用 |
| `GET /api/readers` | リーダー、分類、routes、固定 routes |
| `POST /api/refresh` | リーダーを再走査し、再判定を要求 |

`PUT /api/config` は `{ "address": "0.0.0.0", "port": 6900, "allow": [], "pins": ["", ""], "fixed": [], "verbose": false }` の形を受け付けます。JSON の未知フィールドは拒否します。設定適用に失敗した場合は HTTP 400 で理由を返します。

## 設定と認証

CLI 起動は明示 `--config` 以外の INI を読みません。トレイ起動では標準パスを読み込みます。Web UI 保存先は明示設定ファイル、または OS 標準パスです。起動時に指定した CLI 上書きは保存値に混ぜず、次回起動で再び優先します。旧 INI の `[Web] Token=` は無視し、Web UI 保存時には出力しません。非ループバック待受では認証なしで公開する警告を起動時に記録します。

Web 設定画面には認証もアクセス元検査もなく、カード通信も無認証です。ファイアウォールと `Allow` でアクセス元を管理する前提です。Web HTTP とカード通信は平文です。

## システムトレイ

macOS / Windows は `tray-icon` と `tao` を使い、Linux は GTK に依存しない `ksni` の StatusNotifierItem を使います。Linux のアイコンは PNG を RGBA として読み込み、各ピクセルを ARGB32 (A, R, G, B) に並べて D-Bus へ渡します。クライアント数を250ms周期で確認し、アイコン、ツールチップ、メニュー表示を更新します。メニューの再判定は別スレッドで実行します。バージョン情報はメニュー内にバージョン文字列として表示します。

StatusNotifierWatcher がない場合は警告を出してトレイなしでサーバーを継続します。GNOME では AppIndicator / KStatusNotifierItem 拡張などの StatusNotifierItem ホストが必要です。ksni は D-Bus 接続を Rust 実装し、追加の GTK / AppIndicator 開発パッケージを必要としません。
