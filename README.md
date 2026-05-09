# isunarabe-2026 (team-7)

ISUNARABE 合同演習 2026 の作業リポジトリ。

## 構成

| パス | 内容 |
|---|---|
| `webapp/` | 競技対象 Rust webapp (axum + sqlx + MySQL 8.0)。`/home/isucon/webapp/` と等価 |
| `webapp/sql/seed.sql` | 236M。**.gitignore済み**。`scripts/seed-fetch.sh` で取得 |
| `infra/cloudformation.yml` | 配布された CFn テンプレート (チーム固有、他に共有不可) |
| `scripts/` | 運用スクリプト |

## サーバー

- **nrb2026-1**: `3.115.92.38` (private 192.168.0.11, INSTANCE_INDEX=0)
- **nrb2026-2**: `35.76.59.65` (private 192.168.0.12, INSTANCE_INDEX=1)
- **nrb2026-3**: `35.72.99.228` (private 192.168.0.13, INSTANCE_INDEX=2)
- **bench**: `13.193.122.121` (private 192.168.0.100)

ユーザ: `isucon` (GitHub登録のSSH鍵)

## スクリプト

```sh
scripts/pull.sh [1|2|3]              # リモート webapp/ をローカルへ取得 (default: 1)
scripts/seed-fetch.sh [1|2|3]        # seed.sql (236M) をローカルへ取得 (gitignored)

scripts/deploy.sh [1|2|3 ...]        # ローカル webapp/ → 指定サーバ + restart (default: 全台並列)
PROFILE=release scripts/deploy.sh    # release build (要 unit修正; 後述)

scripts/restart.sh [1|2|3|all]       # nrb2026-webapp.service だけ再起動
scripts/logs.sh [1|2|3]              # journalctl -fu nrb2026-webapp (default: 1)
scripts/status.sh [1|2|3|all]        # systemctl is-active 等
scripts/bench.sh                     # ポータルをブラウザで開く
scripts/bench.sh logs                # bench サーバの benchwarmer ログを tail (走行中の観察用)
```

## ベンチ走行

レギュ §6.2: ベンチは **ポータル経由でしか走行できない** (直接 bench バイナリを叩いてもスコアに反映されない)。

1. `scripts/bench.sh` でポータルを開く → コンテストページの「ベンチマーク実行」
2. target サーバ選択 (1/2/3 切替可)
3. 別ターミナルで `scripts/bench.sh logs` を流しておくとリアルタイムで様子が見れる

## 触っちゃダメ (失格)

- `/etc/systemd/system/isuwari.service` / `/opt/isuwari/` 配下
- `isuadmin` ユーザのアカウント・権限
- フロント静的ファイル (`webapp/public/`)
- インスタンスタイプ変更 / 台数増減 / リージョン変更 / SSH(22)・HTTP(80) のSG

## 改変OK

- DBスキーマ・インデックス・初期データ (`POST /api/initialize` で復元できる範囲)
- ミドルウェア構成 (nginx/MySQL/アプリ配置・台数分担・キャッシュ追加)
- 参考実装の改変・破棄・別言語書き直し (Rustのみ提供)
- OS パッケージ・カーネル設定
- `nrb2026-webapp.service` (isuwari ではない)

## 初手メモ

- 現状 `nrb2026-webapp.service` は `cargo run` の **debug build**。release化は大きい初手
- nginx は未導入。webapp が直接 :80 で listen
- MySQL 8.0 / DB名 `nrb2026` / `mysql_native_password`
- 通知重複は **クリティカルFAIL** (§5.4)。`(user_id, campaign_id)` 単位で高々1回

## 競技後

```sh
aws cloudformation delete-stack --stack-name isunarabe-2026 \
  --profile isunarabe --region ap-northeast-1
```

**ただし追試完了アナウンスがあるまで stack は消さない** (3 台すべて起動状態を維持)。
