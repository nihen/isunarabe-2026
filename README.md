# isunarabe-2026 (team-7)

ISUNARABE 合同演習 2026 の作業リポジトリ。

## トポロジー

| サーバ | Public IP | 役割 | webapp | mysql | DATABASE_URL |
|---|---|---|---|---|---|
| **nrb2026-1** | `3.115.92.38` | app | running | stopped | mysql://...@192.168.0.13:3306 |
| **nrb2026-2** | `35.76.59.65` | app | running | stopped | mysql://...@192.168.0.13:3306 |
| **nrb2026-3** | `35.72.99.228` | app+db | running | running | mysql://...@127.0.0.1:3306 (default) |
| **bench** | `13.193.122.121` | benchmarker | - | - | - |

意図: nrb2026-3 でも webapp を生かしておくことで、bench を nrb2026-3 に向けて打てば `/api/initialize` が **localhost** で完結し、236M の seed.sql ロードが速くなる (ネットワーク往復ゼロ)。通常リクエストは nrb2026-1/2 を bench target にして、リモート DB を使う想定。

## ディレクトリ構成

```
isunarabe/
├── README.md
├── .gitignore
├── infra/
│   └── cloudformation.yml          # 配布CFn (チーム固有、共有不可)
├── webapp/                         # アプリ本体 (nrb2026-1からpull済み)
│   ├── Cargo.toml/lock, rust-toolchain.toml
│   ├── src/main.rs
│   ├── sql/{schema,seed.base}.sql  # seed.sql は gitignore (236M)
│   └── public/                     # SPA。編集禁止
├── etc/                            # /etc/ にミラーされる設定群
│   ├── app/                        # → APP_SERVERS の /etc/
│   │   ├── nginx/
│   │   │   ├── nginx.conf          # ISUCONチューニング済みベース
│   │   │   └── conf.d/nrb2026.conf # vhost (upstream: 127.0.0.1:8080 + app2)
│   │   └── systemd/system/nrb2026-webapp.service.d/
│   │       └── database-url.conf   # DATABASE_URL を 192.168.0.13 に上書き
│   └── db/                         # → DB_SERVER の /etc/
│       └── mysql/mysql.conf.d/
│           └── isucon.cnf          # bind 0.0.0.0, max_connections=1000
└── scripts/
    ├── hosts.sh                    # サーバ・役割定義 (source用)
    ├── pull.sh                     # リモート→ローカル webapp 取得
    ├── seed-fetch.sh               # 236M seed.sql 取得 (gitignored)
    ├── deploy.sh                   # webapp デプロイ + restart (default: 全3台並列)
    ├── deploy-config.sh            # /etc/ ミラー (app/db/both)
    ├── setup-db.sh                 # nrb2026-3 を mysql ホスト化 (webapp も維持)
    ├── setup-app.sh                # nrb2026-1,2 を app 専用化
    ├── setup-topology.sh           # 上2つを順に実行
    ├── restart.sh                  # systemctl restart
    ├── logs.sh                     # journalctl -fu webapp
    ├── status.sh                   # is-active (役割別)
    └── bench.sh                    # ポータル誘導 / benchwarmer ログtail
```

## よく使うコマンド

```sh
# 構成切替 (一回だけ)
scripts/setup-topology.sh           # db -> app の順で適用、最後に status

# 開発ループ
scripts/deploy.sh                   # 全3台にコード反映 + webapp restart
scripts/deploy.sh 1                 # nrb2026-1 だけ
scripts/deploy.sh app               # nrb2026-1,2 のみ

scripts/restart.sh                  # webapp restart
scripts/logs.sh 1                   # nrb2026-1 のログ
scripts/status.sh                   # 役割別の is-active 一覧

# 設定だけ反映
scripts/deploy-config.sh app        # etc/app/ -> /etc/ on APP_SERVERS
scripts/deploy-config.sh db         # etc/db/  -> /etc/ on DB_SERVER
scripts/deploy-config.sh both       # 両方

# ベンチ
scripts/bench.sh                    # ポータルをブラウザで開く
scripts/bench.sh logs               # benchwarmer ログ tail (走行中の観察)

# データ
scripts/seed-fetch.sh               # 236M seed をローカルに (一回だけ)
scripts/pull.sh 1                   # サーバ側のwebapp/をローカルに (緊急時)
```

## etc/ の運用ルール

- `etc/app/<path>` のファイルは **APP_SERVERS の /etc/<path>** に配置される (root所有)
- `etc/db/<path>` のファイルは **DB_SERVER の /etc/<path>** に配置される
- 配置は `rsync --rsync-path="sudo rsync"` で sudo 化。NOPASSWD前提
- ファイルを追加したら `scripts/deploy-config.sh <role>` で反映
- reload/restart はそれぞれの `setup-*.sh` か手動で

## nginx を front に立てる場合 (Phase 2、未実施)

現状 nginx は **未インストール** で、webapp が直接 :80 listen。nginx 構成は git に置いてあるが有効化はまだ。フリップ手順 (将来の作業):

1. `sudo apt install -y nginx` (APP_SERVERS)
2. webapp の PORT を 8080 に変更 (`etc/app/systemd/system/nrb2026-webapp.service.d/port.conf` を作成)
3. `scripts/deploy-config.sh app && sudo systemctl daemon-reload && sudo systemctl restart nrb2026-webapp.service`
4. `sudo rm /etc/nginx/sites-enabled/default`
5. `sudo systemctl reload nginx` (`/etc/nginx/conf.d/nrb2026.conf` が読まれる)

## ベンチ走行 (重要)

レギュ §6.2: ベンチは **ポータル経由のみ**。CLI トリガ不可。

1. `scripts/bench.sh` でポータルを開く → コンテストページの「ベンチマーク実行」
2. target サーバを選ぶ (nrb2026-1 / -2 / -3 切替可)
3. `/api/initialize` を高速に流したいなら nrb2026-3 を target に (mysql localhost)
4. 別ターミナルで `scripts/bench.sh logs` を流して観察

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

## 競技後

```sh
aws cloudformation delete-stack --stack-name isunarabe-2026 \
  --profile isunarabe --region ap-northeast-1
```

**追試完了アナウンスがあるまで stack は消さない** (3台すべて起動状態を維持)。
