# ISUNARABE 合同演習 2026 参戦記 -- 38,300 → 21,087,000 (553倍) の軌跡

## はじめに

[ISUNARABE 合同演習 2026](https://diary.hatenablog.jp/entry/2026/04/13/110000) は「**AI Agent 無制限でチューニングコンテストをやると何が起こるのか見てみたい**」をコンセプトに開催された、ISUCON 非公式模擬大会だ。参考実装が Rust のみという制約の下、AI の使い方そのものが競技力に直結するという異色のルール設計だった。

team-7 として参加し、最終スコアは **21,087,000**。初期スコア 38,300 からの **553倍** まで引き上げた。3台の c5.large (2vCPU)、言語は Rust (Axum + Tokio)。

競合チームのハイスコアは 21,372,000。差はわずか 285,000 (約1.3%) で、惜しくも届かなかった。

本記事では、約8時間の競技時間＋延長戦でのチューニングの全過程を、失敗も含めて振り返る。

## チーム構成と AI 活用

「AI Agent 無制限」の趣旨に全力で乗った。**全作業を Claude Code (Opus 4.6) が主導**し、人間（自分）はレビューと方針決定に徹した。コーディング・perf/alp 分析・ssh でのサーバー操作・デプロイまで、ほぼすべてを AI が行った。

具体的な AI 活用:

- **Claude Code (Opus 4.6)**: メインの開発エージェント。コード実装、perf/alp分析、デプロイまで一貫して担当
- **Oracle (GPT-5.5 Pro)**: セカンドオピニオン。CPU micro-optimizations の知見 (`e775365`) は GPT-5.5 Pro のレビューから得た
- **multi-review-code**: 7エージェント (Claude, Codex, Gemini, Copilot, Cursor Agent, DeepSeek V4, Kimi K2.6) に並列でコードレビューを依頼し、指摘を統合
- **Codex (worktree 並行作業)**: Zig による完全リライトを別 worktree で並行して進めた

AI にやらせて良かった点は、**試行→計測→revert→再試行のサイクルが異常に速い**こと。74コミット中、明示的な Revert が 7回ある。人間がコードを書いていたら、revert の心理的コストで試行回数が半分以下になっていただろう。

## アーキテクチャ概要

スコア計算は `closed campaigns * participants * 1000`。つまり、60秒のベンチマーク中にいかに多くのキャンペーンを close し、各キャンペーンに多くの参加者を join させるかが勝負。

最終構成:

```
bench → nrb2026-1:80 (webapp direct, nginx無効)
         ↓ async write-behind (mpsc)
        nrb2026-3 (MySQL only)

nrb2026-2: 未使用 (nginx proxy のみ、webapp停止)
```

アプリケーションは Rust 単一バイナリ、`webapp/src/main.rs` の 1602行にすべてが収まっている。

## Phase 1-2: 初期セットアップとキャッシュ導入 (12:38 - 13:13)

### 初期状態の把握

最初のコミット `505a8bd` (12:38) で初期セットアップ。3台構成の topology 定義、`/etc` 配下の設定ファイルを git 管理に入れた。

すぐに nginx を導入し (`e83e2b8`)、3台でのロードバランスを試みた。この時点ではまだ「水平分散で勝つ」つもりだった。

### クエリ集約とレスポンスキャッシュ

- `09f6dfe` **phase1**: campaign の N+1 クエリを集約
- `ff65ee8` **phase2**: GET レスポンスのキャッシュ導入
- `7fb5c16` **phase3**: initialize 時にキャッシュをウォームアップ
- `76491e7` **phase4**: join 時のキャッシュ invalidation
- `88034f2` **phase8**: webhook 配信をキューイング

ここまでで基本的な「DB クエリ削減 + レスポンスキャッシュ」の型ができた。

## Phase 3: フル in-memory ストア (14:09)

**転換点**となったのが `bc9787e` — **Phase 3: full in-memory store**。

```
DB からの読み取りを完全に排除し、全データを parking_lot::RwLock<StoreData> で保持。
MySQL は initialize 時のシードロード と 非同期 write-behind のみに使用。
```

これにより hot path から DB アクセスが消え、スコアが大幅に向上した。以降のすべての最適化はこの in-memory アーキテクチャの上に積み上げている。

ただし、DB への write-behind は一筋縄ではいかなかった:

- `f3e72f5` tokio::spawn による async DB write → pool contention でスコア 18M→7M に暴落 → 即 revert (`d35f98a`)
- `5fe6ec4` mpsc チャネルによる dedicated worker に切り替え → 安定

**教訓**: tokio::spawn で DB 書き込みをばらまくと connection pool が枯渇する。専用ワーカーで直列化するのが正解。

## 水平分散の試行と挫折 (14:46 - 15:49)

in-memory ストアができたので、次は2台に水平展開して throughput を倍にしようとした。

### Authority + Read Replica (`2d01fee`)

- nrb2026-1 を authority (write)、nrb2026-2 を read replica とし、内部 sync API でデータを同期
- join/create は authority に、GET は replica に振り分け

**問題**: sync のレイテンシで replica のデータが stale になり、整合性エラーが頻発。sync を同期呼び出しにしても (`ed4c43e`)、nginx の if + proxy_pass のバグ (`a87b8d1`, `e0488c1`) で苦戦。

### Campaign-ID Sharding (`1b90c9f`)

- campaign ID のハッシュで2台に振り分け、各サーバーが担当キャンペーンの join を処理

**問題**: `credit_used` (ユーザーの参加回数上限) の一貫性が保てない。サーバー A で join した分をサーバー B が知らないので、上限を超えて join できてしまう。

### Single Authority 回帰 (`35242ba`)

結局、**single authority に回帰**。2台目の CPU を使えないのは痛いが、一貫性を犠牲にしてスコアが下がるよりはましだ。

この判断が正しかったことは最終スコアが証明している。2vCPU 1台で 21M を出せたのは、以降のマイクロ最適化の積み重ねによる。

## Zig リライト チャレンジ

Rust 版の最適化と並行して、**Codex に worktree で Zig 版の完全リライト**を進めさせた。

- 手書き HTTP サーバー (std.net.Stream ベース)
- 自前 gzip 実装
- 全体を単一の write lock で保護

結果: **15M 前後で頭打ち**。Rust 版の 20M 台に届かなかった。

**敗因分析**:
- Axum の HTTP パース効率が想像以上に高い。手書き HTTP パーサーでは勝てなかった
- `parking_lot::RwLock` の read/write 分離が効いている。Zig 版の全体 write lock では join の並行性が出ない
- Tokio のランタイム最適化 (work-stealing scheduler, io_uring) の恩恵が大きい

ただし Zig 版から得た知見は Rust 版に還元した:
- seed 画像の全サーバー事前配布
- systemd の WorkingDirectory 設定
- deploy.sh のロールバック機能

## DB / OS チューニング

### MySQL 最適化

```ini
innodb_flush_log_at_trx_commit = 2   # fsync を毎コミットではなく1秒ごとに
sync_binlog = 0                       # binlog の fsync も無効化
innodb_buffer_pool_size = 512M        # 2G にしたらメモリプレッシャーで逆効果
disable-log-bin                       # binlog 自体を無効化 (13GB溢れ事件の教訓)
slow_query_log = 0                    # ログ書き込みの I/O 削減
```

binlog が 13GB まで膨らんでディスクを溢れさせたのは本番中のヒヤリハットだった。`disable-log-bin` で根本解決。

### TCP / OS チューニング

```
net.ipv4.tcp_fastopen = 3
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_fin_timeout = 10
net.core.somaxconn = 65535
net.core.netdev_max_backlog = 65535
```

### 不要サービス停止

snapd, ModemManager, polkit, udisks2, multipathd, unattended-upgrades, rsyslog, cron を停止。journald は volatile 化してディスク I/O を排除。THP (Transparent Huge Pages) も `never` に設定して malloc stall を防いだ。

## 最終日の集中チューニング: 21Mの壁との戦い (15:21 - 01:51)

ここからが本番。perf / alp で計測し、oracle / multi-review でレビューし、実装するサイクルを回し続けた。

### alp 分析結果

| エンドポイント | COUNT | SUM(s) | AVG(ms) | P99(ms) |
|---|---|---|---|---|
| GET /api/me | 455,187 | 21,730 | 48 | 169 |
| GET /api/campaigns | 265,907 | 13,327 | 50 | 173 |
| POST /api/campaigns/:id/join | 189,887 | 8,625 | 45 | 172 |

3つのエンドポイントで全リクエストのほぼ 100% を占める。それぞれに特化した最適化が必要。

### perf 分析結果 (242K samples)

| CPU % | Symbol |
|---|---|
| 3.07% | malloc |
| 2.84% | handle_softirqs (kernel) |
| 2.02% | cfree |
| 1.19% | drop_in_place\<AppState\> |
| 1.10% | join_campaign |
| 0.78% | chrono strftime |
| 0.76% | sip::Hasher::write |

**malloc + cfree で 5.09%**。これがアロケーション削減に注力した理由。

### 施策一覧と効果

以下を順次実装した。各施策は perf の特定シンボルを狙い撃ちしている。

#### 1. AppState Arc wrapper (`640326e`)

perf で `drop_in_place<AppState>` が 1.19% を占めていた。Axum は各リクエストで State を clone するため、AppState 全体を `Arc` で包んで clone コストを refcount increment のみにした。

**効果**: CPU 2.67% 削減 (clone + drop)

#### 2. AHash 導入 (`640326e`)

`sip::Hasher::write` が 0.76%。標準の SipHash を AHash に置換。全 HashMap / HashSet を `ahash::AHashMap` / `ahash::AHashSet` に変更。

**効果**: ハッシュ計算の CPU コスト削減

#### 3. 手動 datetime format (`640326e`)

`chrono strftime` が 0.78%。`%Y-%m-%dT%H:%M:%S+09:00` のフォーマットを chrono の strftime ではなく、`[u8; 24]` のスタックバッファに手書きで数字を埋め込む関数に置換。

```rust
fn format_datetime_manual(dt: &NaiveDateTime) -> String {
    let mut buf = [b'0'; 24]; // "2026-05-09T14:09:04+09:00" 相当
    // 年月日時分秒を直接書き込み
    ...
}
```

**効果**: strftime の CPU 0.78% を実質ゼロに

#### 4. user_ids 分離

auth middleware が毎リクエストで `data.read()` を取得してユーザー存在チェックをしていた。ユーザー ID の集合を別の `RwLock<AHashSet<i64>>` に分離し、メインデータの read lock 取得を不要にした。

**効果**: lock contention の大幅削減。GET /api/me と GET /api/campaigns が auth でメインロックを取らなくなった

#### 5. CampaignRes ゼロアロケーション化 (`2fe641a`)

`to_response()` で毎回 String を生成していた全フィールドを Arc 化:

- `name`, `description` → `Arc<str>`
- `tags` → `Arc<[String]>`
- `status` → `&'static str` ("open" / "closed")
- `participants` → `Vec<Arc<ParticipantRes>>`

**効果**: `to_response()` のヒープアロケーションが実質ゼロに。189K 回の join で呼ばれる関数なので影響大

#### 6. list_cache: 参照ソート + top30 のみ変換 (`2fe641a`)

`rebuild_list_cache` で全キャンペーンをソートしていたが、`to_response()` を呼ぶのは上位 30 件のみ。参照 (`&MemCampaign`) でソートしてから top 30 だけ変換するように変更。

#### 7. ImageCache in-memory (`640326e`)

画像をファイルパスだけ持って毎回 disk read していたのを、`Bytes` として直接メモリに保持。seed 画像 + 動的画像の両方を initialize 時にメモリにロード。

#### 8. cache pre-warm (`e7ee8ba`)

initialize 時に me_cache, campaign_json_cache, list_cache を全件構築。ベンチ開始直後のキャッシュミスによるレイテンシスパイクを排除。

#### 9. campaign_json_cache close 時のみ更新

join のたびに campaign の JSON キャッシュを更新していたが、join で変わるのは `current_count` と `participants` だけで、GET /api/campaigns/:id は close 後にしか大量に呼ばれない。close 時のみ更新に変更。

**効果**: 189K 回の join での write lock 取得を排除

#### 10. camp_tag_ids HashSet 遅延構築

join 時のタグマッチングで `HashSet` を構築していたが、タグフィルタ付き saved_search を持つユーザーは全体の 1% 未満。99% の join ではスキップ。

#### 11. UUID 生成を DB worker 側に移動 (`e7ee8ba`)

参加者の UUID 生成を hot path (write lock 内) から DB write-behind worker に移動。write lock の保持時間を短縮。

#### 12. LIST_CACHE_AGGRESSIVE AtomicBool 化

環境変数 `LIST_CACHE_AGGRESSIVE=1` で、list_cache のクリアをキャンペーン close 時のみに限定。`AtomicBool` で lock-free に判定。

## 効果がなかったもの (正直に)

ISUCONでは「何が効かなかったか」も重要な知見。以下は実装して計測した結果、revert または不採用になったもの。

### jemalloc (`bcf53c4` で除去)

malloc が 3.07% なら jemalloc で改善するだろうと思ったが、glibc の malloc と同等かやや悪化。c5.large の 2vCPU では jemalloc の arena 管理オーバーヘッドが逆効果だった可能性がある。

### Pre-gzipped caches (`c2f62df` → `2ed7d21` revert → 再実装 → 最終的に除去)

gzip 済みレスポンスをキャッシュしておけば CompressionLayer をスキップできる......はずだった。しかし **bench が `Accept-Encoding: gzip` を送らないリクエストがある**ことが判明。gzip 無しのレスポンスも用意する必要があり、キャッシュの二重管理のオーバーヘッドが gzip CPU 削減を上回った。

3回 revert して諦めた。

### me_cache lock 分離 + list_cache 差分更新 (`f0df98e` → `6f01332` revert)

me_cache を別の RwLock に分離し、list_cache を差分更新 (全クリアではなく変更キャンペーンのみ更新) する最適化。理論的には正しいが、実測では 20.6M に下降。lock の粒度を細かくしすぎると lock 取得自体のオーバーヘッドが増える好例。

### /api/me の gzip 無効化

gzip 圧縮の CPU を削れると思ったが、レスポンスサイズ増加で network bytes が増え、bench 側の受信時間が伸びて逆効果。

### Fast-path read check in join (`22e7f9f` → `b5df262` revert)

join の 90% は「既に参加済み」「定員満了」で失敗する。write lock を取る前に read lock でチェックすれば 90% のケースで write lock をスキップできる......と思ったが、**read lock → 解放 → write lock の二重取得オーバーヘッド**の方が大きかった。

### MySQL buffer_pool = 2G (`3f39385` で 512M に縮小)

DB サーバーのメモリは 4GB。buffer_pool を 2G にしたらメモリプレッシャーで OS がスワップし始め、逆効果。512M が最適だった。

## ボトルネック分析: 21M の壁

最終的な CPU 使用率は `us=48% sy=17%`。ユーザー空間の CPU はまだ余っているように見えるが、**ベンチマーカーの actor 数が 3174 で固定**されている。

スコア = `closed campaigns * participants * 1000` で、60秒のベンチ中に actor が生成できるリクエスト数には物理的な上限がある。つまり、**サーバー側をどれだけ速くしても、bench 側の actor count に律速される**。

21M の壁を超えるには:
1. レスポンスタイムをさらに削って actor の回転率を上げる
2. global RwLock を DashMap 等に分割して並行性を上げる

2 は最終日の深夜にトライするにはリスクが高すぎた。RwLock のセマンティクスを前提にしたキャッシュ整合性ロジックが全面的に書き変わるため、バグ混入のリスクが高い。

結果として 21,087,000 で着地。競合の 21,372,000 との差は **actor 1回転分**程度で、DashMap 化が間に合っていれば逆転できた可能性はある。

## タイムライン

競技時間は 10:00〜18:00 JST (最後のベンチ結果が採用)。以下は主要な施策のタイムライン。

| 時刻 | フェーズ | 主な施策 |
|------|---------|---------|
| 12:38 | 初期セットアップ | topology定義、git管理 |
| 12:53 | nginx導入 | 3台ロードバランス |
| 13:06 - 13:13 | Phase 1-2 | クエリ集約、レスポンスキャッシュ |
| 13:27 - 13:44 | Phase 5-9 | auth/tagキャッシュ、画像ファイル化 |
| 14:09 | **Phase 3** | **フル in-memory ストア (転換点)** |
| 14:14 - 14:16 | DB write-behind | tokio::spawn → revert → mpsc worker |
| 14:46 - 15:04 | 水平分散 | authority/replica → sharding → 断念 |
| 15:11 - 15:49 | micro-opt | GPT-5.5 Pro レビュー、lock最適化 |
| 16:06 - 16:33 | Arc化 | Arc\<str\>, Arc\<ParticipantRes\>, Bytes |
| 17:09 - 17:45 | gzip戦争 | pre-gzip → revert × 3 → 諦め |
| 18:16 - 18:23 | list cache | dirty flag → revert → clear() |
| 19:10 - 19:35 | 最終最適化 | ahash, manual datetime, zero-alloc, cache pre-warm |

## 学んだこと

### 1. in-memory が正義

ISUCON のワークロードでは、DB はボトルネックになるだけ。全データがメモリに載るなら、DB は永続化のためだけに使うべき。write-behind パターンで非同期に書けばスコアに影響しない。

### 2. 水平分散は「最後の手段」

2台に分散すれば throughput 2倍......にはならない。sync のレイテンシ、一貫性の維持、nginx のルーティング複雑化。single authority で限界まで絞り切ってから考えるべき。

### 3. 計測なき最適化は害

jemalloc、pre-gzip、lock 分離。理論的に正しい最適化が実測で逆効果になるケースが多々あった。**perf → 実装 → bench → 判断** のサイクルを高速に回せる体制が重要。

### 4. AI は「高速な試行」を可能にする

74コミット中 7回の revert。Claude Code が書いて、ベンチで計測して、ダメなら即 revert。このサイクルが人間だけでは不可能な試行回数を実現した。GPT-5.5 Pro のセカンドオピニオン (`e775365`) や、7エージェント並列レビューによる多角的な視点も有効だった。

### 5. malloc を舐めるな

perf の上位 5% が malloc/cfree だった。Arc 化、&'static str 化、スタックバッファ化など、ヒープアロケーションを 1つずつ潰していく地道な作業が最終的なスコアに直結した。

## まとめ

38,300 → 21,087,000。553倍。

13時間で 74コミット、7回の revert、水平分散の断念、Zig リライトの不採用。失敗の数だけ学びがあった。

競合の 21,372,000 には 1.3% 届かなかったが、single authority + parking_lot::RwLock + ゼロアロケーション + AI 駆動の高速サイクルという戦略は、2vCPU 1台で 21M という数字で証明できたと思う。

次があれば、DashMap によるロック分割を初日から仕込んでおきたい。あの 285,000 点の差は、きっとそこにある。
