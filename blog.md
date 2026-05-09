# ISUNARABE 合同演習 2026 参戦記 -- 38,300 → 21,887,000 (572倍) の軌跡

## はじめに

[ISUNARABE 合同演習 2026](https://diary.hatenablog.jp/entry/2026/04/13/110000) は、「**AI Agent 無制限でチューニングコンテストをやると何が起こるのか見てみたい**」をコンセプトに開催された ISUCON 非公式模擬大会だ。

ISUCON は、与えられた Web アプリケーションを決められた時間内で高速化し、ベンチマーカーが出すスコアを競うコンテストである。今回は参考実装が Rust のみで、しかも AI Agent の利用が無制限。つまり、アプリを速くする力だけでなく、**AI にどう調査させ、どう実装させ、どう判断するか**までが競技力になる、かなり異色のルールだった。

team-7 として参加し、最終スコアは **21,887,000**。初期スコア 38,300 から **572倍** まで引き上げた。構成は 3台の c5.large、各 2vCPU。言語は Rust、Web フレームワークは Axum、非同期ランタイムは Tokio だ。

最終結果は 20チーム中 **2位**。ソロ参加での準優勝だった。

この記事では、約8時間の競技時間と延長戦でやったことを振り返る。成功した施策だけでなく、スコアを落とした失敗や、AI とどう相談しながら判断したかも含めて書く。

## チーム構成と AI 活用

今回のテーマは「AI Agent 無制限」だ。ならば中途半端に使っても面白くない。そこで、**全作業を Claude Code (Opus 4.6) 主導**にし、人間である自分はレビューと方針決定に寄せた。

コード実装、`perf` による CPU プロファイル分析、`alp` によるアクセスログ分析、ssh でのサーバー操作、デプロイまで、ほぼすべてを AI が行った。人間がやったのは、「その方向で進めるか」「危ないので戻すか」「今はリスクを取るべきか」を決めることだった。

具体的な AI 活用:

- **Claude Code (Opus 4.6)**: メインの開発エージェント。コード実装、perf/alp分析、デプロイまで一貫して担当
- **Oracle (GPT-5.5 Pro)**: セカンドオピニオン。CPU micro-optimizations の知見 (`e775365`) は GPT-5.5 Pro のレビューから得た
- **multi-review-code**: 7エージェント (Claude, Codex, Gemini, Copilot, Cursor Agent, DeepSeek V4, Kimi K2.6) に並列でコードレビューを依頼し、指摘を統合
- **Codex (worktree 並行作業)**: Zig による完全リライトを別 worktree で並行して進めた

AI との協働で一番効いたのは、**失敗を前提にした試行回数**だった。

たとえば、ある時点で `perf` の結果を Claude Code に渡して「CPU 上位の関数を、スコアに効きそうな順に並べて」と頼む。Claude は `malloc`、`cfree`、`chrono strftime`、`sip::Hasher::write` を拾い、「アロケーション削減、日時フォーマットの手書き化、AHash 導入が候補」と返す。そこで自分が「危険度が低い順にやって。ベンチ結果が落ちたら即 revert」と指示する。

実装後にベンチを回し、伸びれば残す。落ちれば戻す。この会話と判断のサイクルが非常に速かった。

74コミット中、明示的な Revert は 7回ある。人間が自分でコードを書いていたら、revert する心理的コストが高く、ここまで気軽には試せなかったはずだ。AI に任せることで、「雑に試して、数字で判断して、ダメなら捨てる」が徹底できた。

### 実際のプロンプト例

今回の趣旨は「AI Agent 無制限で何が起こるのか」なので、人間が AI にどんな指示を出していたかも載せておく。以下は実際のプロンプトだ。

**初手: コードベースの理解**

> webappを解説して

（→ Codex へ。開始直後、まず Codex にコードの全体像を説明させた。API 仕様、DB スキーマ、認証方式を把握してから手を付ける。）

**全体設計プランの策定**

> このwebappの全エンドポイントとその機能を.plansにかきだして
> → X-User-IDの情報をもとにnginxが水平分散する最適化案はありうるか
> → （水平分散の議論を経て）.plansにかいて！

（→ Claude Code へ。まずエンドポイント一覧を整理させ、水平分散の可否を議論し、最終的に `.plans/webapp-theoretical-fastest-plan.md` という「理論上最速化プラン」を作らせた。515行、Phase 1〜8 に分けた段階的な設計書で、以下が骨子:

1. DB index追加 + SQL集約（安全なベースライン引き上げ）
2. アプリ内 read-through cache
3. **全データ RAM 化（initialize 時にDB→メモリ、以降 DB 読み取りゼロ）**
4. join のロック最適化（user/campaign 単位の shard lock）
5. seed.sql の事前変換（bincode/rkyv snapshot）
6. nginx topology 固定
7. JSON allocation 削減
8. webhook 非同期化

このプランがその後の全作業の骨格になった。実際には Phase 3（フル in-memory ストア）の実装がスコアを一気に押し上げ、Phase 4 の shard lock は DashMap 化のリスクが高く最終的に見送った。プランどおりに進んだ部分と、計測結果を見て方針を変えた部分の両方がある。）

**ログ分析の委任**

> のこされたログからalp, slowquery分析して

（→ Claude Code へ。ssh でサーバーに入り、nginx ログを alp で分析、MySQL のスロークエリログも解析。ボトルネックのエンドポイントとクエリを特定させた。ここから「DB を消す」方針が固まった。）

**Zig リライトの発注**

> brantchきってworktreeで作業はじめて
> → rewrite-zigブランチで
> → zigでwebappをかきなおして。理論上最速の書き方で

（→ Codex へ。3行のプロンプトで Zig 版のフルリライトが始まった。worktree で Rust 版と並行作業。「理論上最速」という曖昧な指示だったが、Codex は手書き HTTP サーバー + 自前 gzip + 全体 write lock で実装してきた。結果は 15M で不採用だったが、こういう大胆な試行を気軽に投げられるのが AI 活用の強みだ。）

**Zig 版のレビュー発注**

> Zig版webappの未コミット差分をレビューしてください。観点: API仕様・正しさ・競合/メモリ安全・キャッシュinvalidation漏れ

（→ Codex へ。Zig 版のコードを別エージェントにレビューさせた。5件の High 指摘が返ってきた — webhook が最初の1ユーザーにしか送信されない、画像 ETag が固定値、seed データの読み込み誤りなど。AI が書いたコードを別の AI にレビューさせる、マルチエージェント体制だ。）

**Oracle への丸投げ**

> oracleのレビュー結果を実装おねがい！

GPT-5.5 Pro (Oracle) のパフォーマンスレビュー結果を Claude Code にそのまま渡して実装させた。レビュー結果には「gzip済みキャッシュ」「list_campaigns参照ソート」「SyncEvent構築スキップ」など優先度付きの提案リストがあり、Claude Code がそれを読んで順次実装した。

**分析依頼**

> alp分析とperf分析したい

Claude Code が ssh でサーバーに入り、nginx を一時的に有効化して LTSV ログを取り、alp を実行。同時に perf record を走らせて CPU プロファイルを取得した。nginx の有効化→ベンチ→分析→nginx 無効化まで一気にやってくれる。

**方針決定**

> コスト気にせずとにかく限界までチューニングしてお願い

perf/alp の結果を踏まえて、Claude Code が自分で優先順位を立てて AppState Arc 化、ahash 導入、手動 datetime フォーマット、user_ids 分離を一括実装した。

**レビュー依頼**

> さっきのalp/perf結果をともなって、multi review, oracleして

7つの AI エージェント (Claude, Codex, Gemini, Copilot, Cursor Agent, DeepSeek V4, Kimi K2.6) に並列でコードレビューを投げつつ、GPT-5.5 Pro にもセカンドオピニオンを求めた。結果を統合して「複数エージェントが共通で指摘した項目」を優先的に実装した。

**戦略相談**

> 21,372,000 他チームのハイスコアだからこれをこえたいんだよね。

このひと言で、Claude Code が残りの最適化候補をスコアインパクト順に整理し直して提案してきた。「me_cache の global lock 解放が最も効果が高い」という分析が返ってきた。

**リスク管理**

> 20時JSTがリミットで、最後のベンチデータが採用されるから、そこ意識して。スコア下がったら戻せるように。

時間制約を伝えると、Claude Code がコミットしてからデプロイする運用に切り替え、revert コマンドを事前に準備した。

**micro opt 依頼**

> なにかほかにできることあるか

Claude Code が「不要サービスの停止」「journald の volatile 化」「THP=never」などの OS レベルのチューニングを提案・実行した。

**ブレーキ**

> ちょっと！ベンチ実行中にdeployしないで！

ベンチ中にデプロイしてスコアを壊した場面。この後 Claude Code は「ベンチ完了を確認してからデプロイ」を学習した。AI は便利だが、暴走を止めるのは人間の仕事だ。

ポイントは、**プロンプトが短い**ことだ。「oracleのレビュー結果を実装おねがい！」で十分に伝わる。なぜなら、Claude Code はこのセッション中のすべてのコンテキスト（コード、perf 結果、過去のベンチスコア、失敗した施策）を持っているからだ。長い指示書を書く必要はなく、方針と判断基準だけ伝えればよかった。

## アーキテクチャ概要

スコア計算は `closed campaigns * participants * 1000`。つまり、60秒のベンチマーク中にできるだけ多くのキャンペーンを close し、それぞれに多くの参加者を join させるゲームだった。

最終構成:

```
bench → nrb2026-1:80 (webapp direct, nginx無効)
         ↓ async write-behind (mpsc)
        nrb2026-3 (MySQL only)

nrb2026-2: 未使用 (nginx proxy のみ、webapp停止)
```

最終的には、Web アプリは nrb2026-1 の 1台だけで動かした。MySQL は nrb2026-3 に分離し、nrb2026-2 はほぼ使わなかった。

`write-behind` は、アプリがリクエストを処理したあと、DB 書き込みを裏側のワーカーに遅延実行させる方式だ。ユーザーへのレスポンスを先に返せるため、ベンチ上の体感速度を上げやすい。

アプリケーションは Rust 単一バイナリで、`webapp/src/main.rs` の 1602行にすべてが収まっている。

## Phase 1-2: 初期セットアップとキャッシュ導入 (12:38 - 13:13)

### 初期状態の把握

最初のコミット `505a8bd` (12:38) で初期セットアップを行った。3台構成の topology 定義と、`/etc` 配下の設定ファイルを git 管理に入れるところから始めた。

この時点では、まだ「3台を全部使えば速くなるだろう」と考えていた。そこで nginx を導入し (`e83e2b8`)、3台ロードバランスを試した。nginx はリクエストを複数のアプリサーバーへ振り分けるリバースプロキシだ。

今振り返ると、この判断はかなり素直だった。ISUCON ではよくある出発点だし、CPU が3台あるなら使いたくなる。ただ、このあとすぐに「水平分散はそんなに簡単ではない」と思い知らされる。

### クエリ集約とレスポンスキャッシュ

最初に取り組んだのは、DB クエリの削減とレスポンスキャッシュだった。キャッシュとは、一度作った結果を保存しておき、同じ処理を繰り返さないようにする仕組みだ。

- `09f6dfe` **phase1**: campaign の N+1 クエリを集約
- `ff65ee8` **phase2**: GET レスポンスのキャッシュ導入
- `7fb5c16` **phase3**: initialize 時にキャッシュをウォームアップ
- `76491e7` **phase4**: join 時のキャッシュ invalidation
- `88034f2` **phase8**: webhook 配信をキューイング

N+1 クエリとは、一覧を1回取得したあと、各行ごとに追加クエリを発行してしまう典型的な遅いパターンだ。まずはこれを潰した。

ここまでで、基本形はできた。DB への問い合わせを減らし、同じレスポンスは再利用する。ISUCON の序盤としては王道の進め方だった。

ただし、まだこの段階では「DB を速く使う」発想だった。この後、発想を変えて「DB を hot path から消す」方向へ進む。

## Phase 3: フル in-memory ストア (14:09)

**最大の転換点**は `bc9787e`、**Phase 3: full in-memory store** だった。

```
DB からの読み取りを完全に排除し、全データを parking_lot::RwLock<StoreData> で保持。
MySQL は initialize 時のシードロード と 非同期 write-behind のみに使用。
```

ここで、アプリの中心を DB からメモリへ移した。`in-memory store` は、必要なデータをすべてプロセス内のメモリに持つ方式だ。`RwLock` は、読み取りは複数同時に許可し、書き込みは1つだけに制限するロックである。

この変更で、通常リクエストの処理中に DB を読まなくなった。DB アクセスは initialize 時の初期ロードと、裏側の非同期書き込みだけになる。ここからスコアが大きく伸びた。

ただし、write-behind は簡単ではなかった。

- `f3e72f5` tokio::spawn による async DB write → pool contention でスコア 18M→7M に暴落 → 即 revert (`d35f98a`)
- `5fe6ec4` mpsc チャネルによる dedicated worker に切り替え → 安定

最初は `tokio::spawn` で DB 書き込みタスクをばらまいた。Tokio は Rust の非同期ランタイムで、軽量タスクを大量に動かせる。だが、DB コネクション数には上限がある。タスクを増やしすぎると、コネクションプールの取り合いが起き、スコアが 18M から 7M まで落ちた。

このときはかなり焦った。ベンチ結果を見て「高速化したはずなのに壊滅している」となり、Claude Code に直前差分とログを見せた。返ってきた仮説は「DB pool contention。書き込みを無制限に spawn しているのが原因」。その場で revert し、mpsc チャネルで専用ワーカーに直列化した。

**教訓**: tokio::spawn で DB 書き込みをばらまくと connection pool が枯渇する。専用ワーカーで直列化するのが正解。

## 水平分散の試行と挫折 (14:46 - 15:49)

in-memory ストアでスコアが伸びたので、次は「2台に水平展開すれば throughput も倍になるのでは」と考えた。水平分散とは、複数のサーバーに処理を分担させる方式だ。

ここからしばらく、かなり泥臭い試行錯誤が始まった。

### Authority + Read Replica (`2d01fee`)

- nrb2026-1 を authority (write)、nrb2026-2 を read replica とし、内部 sync API でデータを同期
- join/create は authority に、GET は replica に振り分け

`authority` は正となるデータを持つサーバー、`read replica` は読み取り専用のコピーを持つサーバーだ。書き込みは authority に集め、GET は replica に逃がせば、負荷が分散できるはずだった。

**問題**: sync のレイテンシで replica のデータが stale になり、整合性エラーが頻発。sync を同期呼び出しにしても (`ed4c43e`)、nginx の if + proxy_pass のバグ (`a87b8d1`, `e0488c1`) で苦戦。

stale とは、古いデータを見てしまうことだ。join 直後の状態が replica に反映される前に GET が飛んでくると、ベンチマーカーから見ると矛盾したレスポンスになる。性能以前に正しさが崩れるので、これは採用できなかった。

### Campaign-ID Sharding (`1b90c9f`)

- campaign ID のハッシュで2台に振り分け、各サーバーが担当キャンペーンの join を処理

次に、campaign ID ごとに担当サーバーを分ける sharding を試した。sharding は、データをキーごとに分割し、それぞれ別サーバーで処理する方法だ。

一見よさそうに見えたが、すぐに別の整合性問題が出た。

**問題**: `credit_used` (ユーザーの参加回数上限) の一貫性が保てない。サーバー A で join した分をサーバー B が知らないので、上限を超えて join できてしまう。

campaign 単位では分割できても、ユーザー単位の制約が横断している。これを正しく扱うには、ユーザー状態を同期するか、ユーザー単位で shard する必要がある。だが、それをやると今度は campaign 側の制約と衝突する。

このあたりで「2台使えば勝てる」という楽観が消えた。

### Single Authority 回帰 (`35242ba`)

結局、**single authority に回帰**した。Web アプリは1台で動かし、他の台は補助に回す方針だ。

2台目の CPU を使えないのは痛い。だが、整合性エラーでスコアが落ちるよりはましだった。ISUCON では、速くても壊れている実装は意味がない。ベンチマーカーに怒られない範囲で、どこまで速くできるかが勝負だ。

この判断は苦しかったが、結果的には正しかった。2vCPU 1台で 21M まで伸ばせたのは、この後のマイクロ最適化を single authority に集中できたからだ。

## Zig リライト チャレンジ

Rust 版の最適化と並行して、**Codex に worktree で Zig 版の完全リライト**を進めさせた。

Zig は低レイヤー寄りのシステムプログラミング言語で、メモリ管理や HTTP 処理をかなり細かく制御できる。Rust + Axum の抽象化コストを避ければ、もっと速くなる可能性があると考えた。

- 手書き HTTP サーバー (std.net.Stream ベース)
- 自前 gzip 実装
- 全体を単一の write lock で保護

結果: **15M 前後で頭打ち**。Rust 版の 20M 台には届かなかった。

**敗因分析**:
- Axum の HTTP パース効率が想像以上に高い。手書き HTTP パーサーでは勝てなかった
- `parking_lot::RwLock` の read/write 分離が効いている。Zig 版の全体 write lock では join の並行性が出ない
- Tokio のランタイム最適化 (work-stealing scheduler, io_uring) の恩恵が大きい

これは少し意外だった。自前実装なら速くなると思っていたが、Axum/Tokio の完成度が高かった。特に、HTTP パースと非同期 I/O の部分で、簡単な手書き実装では勝てない。

ただし Zig 版は無駄ではなかった。Rust 版とは別の視点で問題を見たことで、いくつかの知見を持ち帰れた。

- seed 画像の全サーバー事前配布
- systemd の WorkingDirectory 設定
- deploy.sh のロールバック機能

採用されなかったリライトでも、周辺改善の種は拾えた。これも AI に並行作業を任せたからできたことだ。

## DB / OS チューニング

### MySQL 最適化

```ini
innodb_flush_log_at_trx_commit = 2   # fsync を毎コミットではなく1秒ごとに
sync_binlog = 0                       # binlog の fsync も無効化
innodb_buffer_pool_size = 512M        # 2G にしたらメモリプレッシャーで逆効果
disable-log-bin                       # binlog 自体を無効化 (13GB溢れ事件の教訓)
slow_query_log = 0                    # ログ書き込みの I/O 削減
```

MySQL は hot path から外したが、write-behind の書き込み先としてはまだ重要だった。そこで、耐障害性よりベンチ中の速度を優先する設定に寄せた。

`fsync` は、データをディスクへ確実に書き込む処理だ。安全性は上がるが I/O コストが高い。競技中は永続性よりスコアが重要なので、毎コミット fsync しない設定にした。

一番ヒヤリとしたのは、binlog が 13GB まで膨らんでディスクを溢れさせた事件だ。binlog は MySQL の更新履歴ログで、レプリケーションや復旧に使われる。今回は不要だったので、`disable-log-bin` で根本的に止めた。

### TCP / OS チューニング

```
net.ipv4.tcp_fastopen = 3
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_fin_timeout = 10
net.core.somaxconn = 65535
net.core.netdev_max_backlog = 65535
```

TCP 周りも、短時間に大量のリクエストを受けるベンチ向けに調整した。

`somaxconn` は接続待ち行列の上限、`netdev_max_backlog` はネットワークパケット処理の待ち行列に関わる設定だ。ここを広げて、瞬間的なリクエスト増に詰まりにくくした。

### 不要サービス停止

snapd, ModemManager, polkit, udisks2, multipathd, unattended-upgrades, rsyslog, cron を停止。journald は volatile 化してディスク I/O を排除。THP (Transparent Huge Pages) も `never` に設定して malloc stall を防いだ。

THP は大きなメモリページを使って性能を上げる仕組みだが、タイミングによってはメモリ確保時に詰まることがある。今回はレイテンシのブレを嫌って無効化した。

このあたりは劇的な一撃ではないが、ノイズを減らすための地ならしだった。

## 最終日の集中チューニング: 21Mの壁との戦い (15:21 - 01:51)

ここからが本番だった。水平分散は諦めた。Zig リライトも届かない。残された道は、Rust 版 single authority を限界まで絞ることだった。

やり方はシンプルだ。`perf` で CPU の使い道を見る。`alp` で遅いエンドポイントを見る。Oracle や multi-review にレビューを投げる。実装する。ベンチを回す。落ちたら戻す。

地味だが、このループが一番強かった。

### alp 分析結果

| エンドポイント | COUNT | SUM(s) | AVG(ms) | P99(ms) |
|---|---|---|---|---|
| GET /api/me | 455,187 | 21,730 | 48 | 169 |
| GET /api/campaigns | 265,907 | 13,327 | 50 | 173 |
| POST /api/campaigns/:id/join | 189,887 | 8,625 | 45 | 172 |

`alp` はアクセスログを集計し、どのエンドポイントがどれだけ呼ばれ、どれだけ時間を使っているかを見るツールだ。

結果を見ると、ほぼすべてのリクエストがこの3つに集中していた。つまり、全体をなんとなく速くするより、この3つの hot path を徹底的に削る方が効く。

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

`perf` は、CPU がどの関数に時間を使っているかを調べるプロファイラだ。

ここで目立ったのが **malloc + cfree で 5.09%** という数字だった。`malloc` はヒープメモリの確保、`cfree` は解放だ。つまり、リクエスト処理そのものだけでなく、メモリ確保と解放にもかなり CPU を使っていた。

ここから方針が決まった。DB はもう消した。次はアロケーションを消す。

### 施策一覧と効果

以下を順次実装した。どれも思いつきではなく、perf の特定シンボルを狙い撃ちしたものだ。

#### 1. AppState Arc wrapper (`640326e`)

perf で `drop_in_place<AppState>` が 1.19% を占めていた。Axum は各リクエストで State を clone するため、AppState 全体を `Arc` で包み、clone コストを refcount increment のみにした。

`Arc` は参照カウント付きの共有ポインタだ。データ本体をコピーせず、参照だけを増やして共有できる。

**効果**: CPU 2.67% 削減 (clone + drop)

#### 2. AHash 導入 (`640326e`)

`sip::Hasher::write` が 0.76%。標準の SipHash は安全寄りのハッシュ関数だが、競技中の HashMap では速度優先でよい。全 HashMap / HashSet を `ahash::AHashMap` / `ahash::AHashSet` に変更した。

**効果**: ハッシュ計算の CPU コスト削減

#### 3. 手動 datetime format (`640326e`)

`chrono strftime` が 0.78%。日時フォーマットは地味だが、呼び出し回数が多いと効いてくる。`%Y-%m-%dT%H:%M:%S+09:00` を chrono の汎用フォーマッタで作るのをやめ、`[u8; 24]` のスタックバッファに手で数字を埋めた。

```rust
fn format_datetime_manual(dt: &NaiveDateTime) -> String {
    let mut buf = [b'0'; 24]; // "2026-05-09T14:09:04+09:00" 相当
    // 年月日時分秒を直接書き込み
    ...
}
```

汎用処理は便利だが、固定フォーマットなら専用実装の方が速い。

**効果**: strftime の CPU 0.78% を実質ゼロに

#### 4. user_ids 分離

auth middleware が毎リクエストで `data.read()` を取得し、ユーザー存在チェックをしていた。middleware はリクエストの前処理を行う層で、ここでは認証処理を担当していた。

ユーザー ID の集合を別の `RwLock<AHashSet<i64>>` に分離し、メインデータの read lock 取得を不要にした。

**効果**: lock contention の大幅削減。GET /api/me と GET /api/campaigns が auth でメインロックを取らなくなった

#### 5. CampaignRes ゼロアロケーション化 (`2fe641a`)

`to_response()` で毎回 String を生成していた全フィールドを Arc 化した。ゼロアロケーションとは、処理中に新しいヒープメモリ確保をできるだけ発生させない設計のことだ。

- `name`, `description` → `Arc<str>`
- `tags` → `Arc<[String]>`
- `status` → `&'static str` ("open" / "closed")
- `participants` → `Vec<Arc<ParticipantRes>>`

**効果**: `to_response()` のヒープアロケーションが実質ゼロに。189K 回の join で呼ばれる関数なので影響大

#### 6. list_cache: 参照ソート + top30 のみ変換 (`2fe641a`)

`rebuild_list_cache` で全キャンペーンをソートし、全件をレスポンス形式へ変換していた。しかし実際に返すのは上位 30 件だけだった。

そこで、まず `&MemCampaign` の参照だけをソートし、top 30 だけ `to_response()` するように変更した。データ本体を動かさず、必要な分だけ変換する方針だ。

#### 7. ImageCache in-memory (`640326e`)

画像をファイルパスだけ持ち、リクエストごとに disk read していた。これをやめて、`Bytes` として直接メモリに保持した。

seed 画像と動的画像の両方を initialize 時にメモリへロードすることで、画像配信時のディスク I/O を消した。

#### 8. cache pre-warm (`e7ee8ba`)

initialize 時に me_cache, campaign_json_cache, list_cache を全件構築した。

pre-warm は、ベンチ開始前にキャッシュを事前構築しておくことだ。これにより、ベンチ開始直後のキャッシュミスによるレイテンシスパイクを避けられた。

#### 9. campaign_json_cache close 時のみ更新

join のたびに campaign の JSON キャッシュを更新していたが、join で変わるのは `current_count` と `participants` だけだった。しかも、GET /api/campaigns/:id は close 後に大量に呼ばれる傾向があった。

そこで、join 時の更新をやめ、close 時のみ更新するようにした。

**効果**: 189K 回の join での write lock 取得を排除

#### 10. camp_tag_ids HashSet 遅延構築

join 時のタグマッチングで `HashSet` を構築していたが、タグフィルタ付き saved_search を持つユーザーは全体の 1% 未満だった。

そこで、必要なケースだけ `HashSet` を作る遅延構築に変更した。99% の join では余計な構築をスキップできる。

#### 11. UUID 生成を DB worker 側に移動 (`e7ee8ba`)

参加者の UUID 生成を hot path、つまり write lock 内から外した。代わりに DB write-behind worker 側で生成するようにした。

UUID は一意な ID だが、生成処理にもコストがある。ロック中にやる必要はないので、後ろへ逃がした。

#### 12. LIST_CACHE_AGGRESSIVE AtomicBool 化

環境変数 `LIST_CACHE_AGGRESSIVE=1` で、list_cache のクリアをキャンペーン close 時のみに限定した。

判定には `AtomicBool` を使った。Atomic はロックなしで安全に読み書きできる値で、ここでは設定値のチェックを lock-free にした。

## 効果がなかったもの (正直に)

ISUCON では「何が効かなかったか」も重要な知見だ。以下は実装して計測した結果、revert または不採用になったもの。

### jemalloc (`bcf53c4` で除去)

malloc が 3.07% なら効くだろうと思ったが、glibc malloc と同等かやや悪化。2vCPU では jemalloc の arena 管理コストが勝った可能性がある。

### Pre-gzipped caches (`c2f62df` → `2ed7d21` revert → 再実装 → 最終的に除去)

gzip 済みレスポンスをキャッシュして CompressionLayer を避ける狙いだったが、gzip 非対応リクエストもあり二重管理が必要になった。キャッシュ管理コストが CPU 削減を上回り、3回 revert して諦めた。

### me_cache lock 分離 + list_cache 差分更新 (`f0df98e` → `6f01332` revert)

理論上は lock contention を減らせるはずだったが、実測では 20.6M に下降。lock を細かくしすぎると、取得回数そのものが負担になる。

### /api/me の gzip 無効化

gzip の CPU を削る狙いだったが、レスポンスサイズが増えて network bytes が増加。bench 側の受信時間が伸び、逆効果だった。

### Fast-path read check in join (`22e7f9f` → `b5df262` revert)

失敗 join を read lock だけで弾く狙いだったが、read lock → write lock の二重取得が重かった。シンプルに write lock を取る方が速かった。

### MySQL buffer_pool = 2G (`3f39385` で 512M に縮小)

DB サーバーのメモリは 4GB。buffer_pool 2G はメモリプレッシャーを招き、スワップ気味になって逆効果。512M が安定した。

## ボトルネック分析: 21M の壁

最終的な CPU 使用率は `us=48% sy=17%`。ユーザー空間 CPU だけ見ると、まだ余裕があるように見える。

しかし、ここで別の壁が見えてきた。**ベンチマーカーの actor 数が 3174 で固定**されていたのだ。

actor は、ベンチマーカー側でユーザー行動を模倣する実行単位だ。サーバーがどれだけ速くても、actor がリクエストを投げてレスポンスを受け取り、次の行動へ進むまでには限界がある。

スコア = `closed campaigns * participants * 1000` で、60秒のベンチ中に actor が生成できるリクエスト数にも物理的な上限がある。つまり、**サーバー側を速くしても、最後は bench 側の actor count に律速される**。

21M の壁を超えるには:
1. レスポンスタイムをさらに削って actor の回転率を上げる
2. global RwLock を DashMap 等に分割して並行性を上げる

DashMap は、HashMap を分割ロックで並行アクセスしやすくするデータ構造だ。global RwLock をやめれば、join の並行性は上がる可能性があった。

だが、最終日の深夜にやるにはリスクが高すぎた。今の実装は RwLock のセマンティクスを前提に、キャッシュ整合性や更新順序を組んでいた。DashMap 化すると、その前提がかなり崩れる。バグを入れれば、スコアどころか通らなくなる。

結果として 21,887,000 で着地し、最終順位は **2位**。

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

ISUCON のような短時間ベンチでは、全データがメモリに載るなら DB は hot path から消すべきだ。DB は永続化のためだけに使い、読み取りは in-memory store に寄せる。今回の最大の伸びはここから来た。

### 2. 水平分散は「最後の手段」

2台に分散すれば throughput 2倍、とはならない。同期レイテンシ、一貫性、ルーティングの複雑化が一気に増える。まず single authority で限界まで絞り切り、それでも CPU が本当に足りないときに考えるべきだった。

### 3. 計測なき最適化は害

jemalloc、pre-gzip、lock 分離。理論的に正しそうな施策でも、実測では逆効果になることが何度もあった。**perf → 実装 → bench → 判断** のサイクルを崩さないことが重要だ。

### 4. AI は「高速な試行」を可能にする

74コミット中 7回の revert。Claude Code が書き、ベンチで測り、ダメなら即戻す。このサイクルが、人間だけでは難しい試行回数を実現した。

特に有効だったのは、AI に「答え」を出させるのではなく、「仮説の候補」を大量に出させる使い方だった。GPT-5.5 Pro のセカンドオピニオン (`e775365`) や、7エージェント並列レビューも、見落としを減らすのに効いた。最後に採用するかどうかは、ベンチ結果と人間の判断で決める。この分担がよかった。

### 5. malloc を舐めるな

perf の上位 5% が malloc/cfree だった。Arc 化、&'static str 化、スタックバッファ化など、ヒープアロケーションを1つずつ潰す地味な作業が、最終スコアに直結した。

大きな設計変更だけが高速化ではない。最後の 1% は、こういう細かい削り込みの積み重ねだった。

## まとめ

38,300 → 21,887,000。572倍。

8時間の競技で 74コミット、7回の revert、水平分散の断念、Zig リライトの不採用。きれいな一本道ではなく、むしろ失敗だらけだった。

それでも、失敗するたびに計測して、原因を見て、次の手を打った。tokio::spawn で DB pool が詰まったら mpsc worker にする。水平分散で整合性が壊れたら single authority に戻る。pre-gzip が落ちたら revert する。AI Agent を使ったことで、この切り替えをかなり高速に回せた。

single authority + parking_lot::RwLock + ゼロアロケーション + AI 駆動の高速サイクルという戦略は、2vCPU 1台で 21.9M という数字で証明できた。

ソロ参加、AI Agent 全力活用で、20チーム中2位。十分に戦えた。
