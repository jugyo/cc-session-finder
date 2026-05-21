# cc-session-finder 実装計画

## Context

Claude Code は `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` の形式で
セッションを保存しているが、`claude --resume` のインタラクティブ・ピッカー
は時系列順の単純な一覧で、過去のセッションをキーワード/意味で素早く探すには
不十分。

`cc-session-finder` は、ローカルに既に存在する全 JSONL セッション (現在 341 件
/ 72MB) を SQLite に索引化し、FTS5 によるキーワード検索とローカル多言語埋め
込みによる意味検索を **並行** に走らせる TUI ファインダーを提供する。選択した
セッションは `claude --resume <session-id>` でそのまま開ける。

成功条件:
- 起動から最初の一覧表示まで **< 100ms** (ウォーム時)
- キー入力 → 一覧更新まで **< 16ms** (キーワード経路)
- 意味検索結果は遅れて下に追記され、ラベルで識別できる
- 日本語クエリで日本語タイトル/プロンプトを引ける
- 起動直後はカレントディレクトリのセッションが最上位

## アーキテクチャ

### スタック
- 言語: **Rust** (単一バイナリ、`cargo install --path .` で配布)
- TUI: **ratatui** + **crossterm** (差分描画、Unicode 幅対応 → `unicode-width`)
- DB: **rusqlite** + **sqlite-vec** クレート (vec0 仮想テーブル静的リンク)
- 全文検索: SQLite **FTS5** (`tokenize='unicode61 remove_diacritics 0'`)
- 埋め込み: **fastembed-rs** + `paraphrase-multilingual-MiniLM-L12-v2` (384次元、
  ~120MB、初回 DL 後 `~/.cache/cc-session-finder/models/` にキャッシュ)
- 非同期: **tokio** (multi-thread runtime) — UI スレッドと検索ワーカーを分離

### データレイアウト

DB パス: `~/.cache/cc-session-finder/index.db`

```sql
CREATE TABLE sessions (
  session_id    TEXT PRIMARY KEY,         -- UUID
  project_dir   TEXT NOT NULL,            -- encoded dir name (~/.claude/projects/<dir>)
  cwd           TEXT NOT NULL,            -- 復元した実 cwd
  ai_title      TEXT,                     -- 最新の ai-title
  first_prompt  TEXT,                     -- 最初のユーザー text (~500 char)
  preview       TEXT,                     -- title + first_prompt 結合 (検索用)
  mtime         INTEGER NOT NULL,         -- ファイル mtime (unix秒)
  size          INTEGER NOT NULL,         -- ファイルサイズ (差分検知用)
  msg_count     INTEGER,                  -- user/assistant メッセージ数
  embedded_at   INTEGER                   -- 埋め込み生成時刻 (NULL=未生成)
);
CREATE INDEX idx_sessions_mtime ON sessions(mtime DESC);
CREATE INDEX idx_sessions_cwd   ON sessions(cwd);

-- FTS5: preview 列を unicode61 でトークナイズ。日本語は trigram 補助で対応:
CREATE VIRTUAL TABLE sessions_fts USING fts5(
  preview,
  content='sessions', content_rowid='rowid',
  tokenize='trigram'  -- 日本語/中文/CJK でも substring 検索可能
);
-- INSERT/UPDATE/DELETE トリガで sessions と同期

-- ベクトル: sqlite-vec の vec0
CREATE VIRTUAL TABLE sessions_vec USING vec0(
  session_id TEXT PRIMARY KEY,
  embedding  FLOAT[384]
);
```

`tokenize='trigram'` を使うことで日本語など空白で区切れない言語でも substring
類似のマッチが効く (FTS5 標準機能、外部辞書不要)。

### モジュール構成

```
cc-session-finder/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI エントリ (clap)
│   ├── paths.rs             # cwd <-> project_dir 変換
│   ├── session.rs           # JSONL パース → SessionRecord
│   ├── index/
│   │   ├── mod.rs           # 公開 API: open(), scan_and_update()
│   │   ├── schema.rs        # マイグレーション
│   │   ├── ingest.rs        # 差分インデックスループ
│   │   └── embed.rs         # fastembed ラッパ + バッチ生成
│   ├── search/
│   │   ├── mod.rs           # SearchEngine (keyword/vector を並行起動)
│   │   ├── keyword.rs       # FTS5 クエリ
│   │   └── vector.rs        # vec0 KNN クエリ
│   ├── tui/
│   │   ├── mod.rs           # event loop (tokio::select)
│   │   ├── app.rs           # State (results, query, selected_idx, ...)
│   │   ├── view.rs          # ratatui 描画
│   │   └── input.rs         # マルチバイトクエリ編集 (`tui-input` 検討)
│   └── launch.rs            # claude --resume の exec
└── docs/
    └── plan.md              # 本ドキュメントのコピー (実装後に作成)
```

### 起動シーケンス

1. `main` で DB を open → 必要なら `CREATE TABLE` 一式 (idempotent)。
2. `scan_and_update()` をバックグラウンドタスクで spawn:
   - `~/.claude/projects/*/*.jsonl` を glob
   - 各ファイル: `(path, mtime, size)` を取り、DB の同 session_id 行と比較
   - 変化があれば JSONL の **末尾優先パース** で `ai-title` を取得、頭から
     探索して最初の text 型ユーザーメッセージを抽出 (`<ide_opened_file>` で
     始まるものはスキップ)、`sessions` を UPSERT
   - 埋め込み未生成・もしくは `preview` が変化した行を集めてキューに投入
3. メインスレッドは即座に TUI を起動:
   - 初回クエリ: `SELECT ... ORDER BY (cwd = ?) DESC, mtime DESC LIMIT N`
   - `N` = 端末の利用可能行数 (ratatui `Frame::area().height` から決定)
   - **インデックス構築/更新中は画面上部にステータスバーを表示し、ユーザー
     入力 (クエリ編集、選択移動、Enter) を一切ブロックする** (詳細は
     「インデックス進行中の UI」セクション参照)
4. 埋め込みワーカー (別タスク): キューを 16 件単位でバッチ embed → `vec0`
   に upsert。ユーザーがクエリを打っている最中でも非同期で進む。

### インデックス進行中の UI

インデックス構築は **2 つのフェーズ** で進行し、それぞれ独立に UI に状態を
伝える:

| フェーズ           | 内容                                  | ブロッキング |
| ------------------ | ------------------------------------- | ------------ |
| `Scanning`         | JSONL の差分検出 + メタデータ抽出 (タイトル・first_prompt・mtime を DB へ) | **ブロック** |
| `Embedding`        | 未生成行のベクトルをバッチ生成        | **非ブロック** |

理由: メタデータが揃わないと一覧表示自体が空 (もしくは不完全) になるため
`Scanning` 中は操作を許してもユーザーが見ているものと選択結果が乖離する。
一方 `Embedding` は意味検索を強化するだけで、キーワード検索とカレント cwd
表示は機能するので非ブロックで OK。

**`Scanning` 中の UI**:
- 画面全体に「DB の更新中…」インジケータを表示
- 進捗バー (`{処理済}/{総ファイル数}`) と現在処理中のプロジェクト名を表示
- スピナー (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` を 80ms 間隔でローテーション)
- **キー入力は捨てる** (`Esc` / `Ctrl-C` のみ受け付けて中断終了)
- 完了したらインジケータを消し、通常 UI (検索ボックス + 結果リスト) に遷移
- ウォーム起動 (差分なし) では `Scanning` が < 100ms で終わる見込みなので
  そもそも視認できず実質的に発生しないが、ロジック上は同じ経路を通る

**`Embedding` 中の UI**:
- 検索ボックス右端に小さなインジケータ (`⠋ embed 23/87` のような短い表記)
- 入力もカーソル移動も通常通り受け付ける
- 完了率に応じてベクトル検索結果がだんだん充実していく (ヒット数で実感できる)
- 完了するとインジケータが消える

**進捗チャネル**: `tokio::sync::watch::channel<IndexProgress>` でワーカー
→ UI へ状態を push する。`IndexProgress` は:

```rust
enum IndexProgress {
    Idle,
    Scanning { done: u32, total: u32, current: String },
    Embedding { done: u32, total: u32 },
}
```

UI イベントループは `tokio::select!` で stdin と watch の両方を待ち、
`Scanning` フェーズ中は入力イベントを drop (Esc/Ctrl-C 除く)。

### 検索フロー (キー入力 1 回ごと)

```
on_query_change(q):
  cancel_in_flight()
  if q.is_empty():
    results = SELECT ... ORDER BY (cwd = ?) DESC, mtime DESC LIMIT visible
    render(results, no spinner)
    return

  # 経路 A: キーワード (同期、~ms)
  kw_hits = fts5_query(q)            # MATCH 'q*' を trigram で
  rank kw_hits by:
    - cwd match boost (current cwd と一致なら +1.0)
    - mtime newness (log-decay)
    - FTS5 bm25
  render(kw_hits with labels)         # 上に [kw] / [cwd] ラベル付き

  # 経路 B: ベクトル (非同期、~50–200ms)
  spawn:
    qv = embed(q)
    sem_hits = vec0_knn(qv, k=visible*2)
    diff = sem_hits - kw_hits         # キーワードと重複しないもの
    append_below(diff with [~] label)
```

並行性は `tokio::select!` でキャンセル可能に。新しいキー入力が来たら旧
タスクを drop してリソース解放。

### 結果アイテムのラベリング

優先順位の高いものから:

| ラベル        | 条件                                              | 色 (ratatui)    |
| ------------- | ------------------------------------------------- | --------------- |
| `[cwd]`       | session の `cwd` が現在の cwd と一致              | green           |
| `[kw]`        | FTS5 でクエリにヒット                             | yellow          |
| `[~]`         | ベクトル KNN でクエリにヒット (kw に未掲載のもの) | cyan (dim)      |
| `[recent]`    | クエリ空・最新ソート時のデフォルト               | gray            |

複数該当する場合は併記 (例: `[cwd][kw]`)。

### 行表示フォーマット

```
[cwd][kw]  2026-05-19 18:48  bringout/seci-cli       Debug seci GraphQL query …
[~]        2026-05-13 09:30  bringout/research        投資戦略の比較分析 …
```

- 日時 (mtime, 短縮形)
- プロジェクトの末尾 2 階層 (`{parent}/{leaf}`)
- AI タイトル (なければ first_prompt の先頭) を端まで省略表示

Unicode 幅は `unicode-width::UnicodeWidthStr` で算出 (日本語は 2 幅)。

### 起動・選択動作

- `↑` / `↓` または `Ctrl-P` / `Ctrl-N` で選択移動
- `Enter` で確定: TUI を tear-down し、選択行の `cwd` に `chdir` してから
  `execvp("claude", ["claude", "--resume", session_id])` で置換実行
  - これにより `claude` プロセスはそのターミナルを引き継ぐ
- `Esc` または `Ctrl-C` でキャンセル終了 (exit 130)

## パス・エンコーディング

Claude Code の project_dir 名は `cwd` を以下で変換した文字列と思われる:
- `/` → `-`
- `.` → `-`

例: `/Users/jugyo/.claude` → `-Users-jugyo--claude`、
`/Users/jugyo/workspace/jugyo/cc-session-finder` →
`-Users-jugyo-workspace-jugyo-cc-session-finder`

**逆変換は曖昧** (`bringout-infra` の `-` がパス区切りか文字か判別不能) なので、
本ツールでは:
1. 索引時に JSONL を読みつつ、各レコード内の `cwd` フィールド (もし含まれて
   いれば) を直接保存する。
2. それが取れない場合のフォールバックとして、ディレクトリ名の単純復元
   (`-` → `/`、連続 `-` で `.` 復元) を試み、実在チェックする。
3. それでも決まらない場合は、ディレクトリ名そのものを `cwd` として保存
   (表示は劣化するが検索は機能)。

カレント cwd マッチは **forward 方向** (現在 cwd を encode して dir 名と直接
比較) で判定するので曖昧性の影響を受けない。

## 主要な使用クレート

| 用途           | クレート                              |
| -------------- | ------------------------------------- |
| TUI            | `ratatui`, `crossterm`                |
| 入力編集       | `tui-input` (IME/UTF-8 セーフ) または自作 |
| Unicode 幅     | `unicode-width`, `unicode-segmentation` |
| 非同期         | `tokio` (`rt-multi-thread`, `macros`) |
| SQLite         | `rusqlite` (`bundled`)                |
| ベクトル拡張   | `sqlite-vec` (Rust binding、静的)     |
| 埋め込み       | `fastembed`                           |
| JSON           | `serde`, `serde_json`                 |
| ファジー補助   | `nucleo-matcher` (FTS5 にヒットしない時のフォールバック) |
| CLI            | `clap` (derive)                       |
| ログ           | `tracing`, `tracing-subscriber`       |
| エラー         | `anyhow`, `thiserror`                 |
| 時刻           | `time`                                |

## 性能見積もり (ユーザー要件: MUST 高速)

| フェーズ                       | 想定                              |
| ------------------------------ | --------------------------------- |
| 初回起動 (DB 空) のインデックス | 341 ファイル全パース ~2–4 秒、 埋め込み生成 ~20–40 秒。 **UI はパース完了を待たない** ので体感ブロックなし。 |
| 2 回目以降の起動              | mtime チェックのみ → 数十 ms。 結果一覧 SELECT は < 5ms。 |
| キーワード検索               | FTS5 trigram (~341 行) → ~1ms。 |
| ベクトル検索                  | 384次元 × 341 件の KNN → ~3–5ms (sqlite-vec)。 embed(q) が ~50–150ms で支配的。 |
| 描画                          | ratatui 差分描画 → < 5ms。 |

## CLI 仕様

引数なしで起動すると TUI モード。サブコマンドを与えると非対話 CLI モードと
なり、AI エージェント/スクリプトから扱いやすい構造化出力を返す。両モード
とも同じ DB・インデックスロジックを共有する。

### TUI モード

```
cc-session-finder [OPTIONS] [QUERY]

OPTIONS:
  --reindex      強制全再構築 (DB をクリアして全ファイル再読み込み)
  --no-vector    ベクトル経路を無効化 (CI 等 embed モデルが無い環境用)
  --limit N      初期表示行数の上限 (デフォルト: 端末高さ)
```

引数 `QUERY` を渡すと、その文字列で初期フィルタした状態で TUI を起動。

### 非対話 CLI モード (AI エージェント向け)

サブコマンドが指定されたとき、または stdout が TTY でないときに自動的に
有効になる。デフォルトで構造化出力 (JSON) を返し、終了コードで成否を伝える。

```
cc-session-finder search [OPTIONS] <QUERY>
  クエリで検索し結果を返す。TUI と同じ keyword/vector/cwd-boost ロジック。
  OPTIONS:
    --mode keyword|vector|both   検索経路 (デフォルト: both)
    --limit N                    返却件数 (デフォルト: 20)
    --cwd <path>                 cwd マッチに使う基準 (デフォルト: $PWD)
    --cwd-only                   その cwd のセッションのみに限定
    --format json|tsv|ids        出力形式 (デフォルト: json)
    --no-update                  事前のインデックス更新をスキップ (キャッシュ即読み)

cc-session-finder list [OPTIONS]
  クエリなしの一覧 (新しい順)。`search ""` と同等だが意味検索は走らない。
  OPTIONS:
    --limit N      (デフォルト: 50)
    --cwd <path>   cwd マッチ基準
    --cwd-only
    --since <duration>   例: 7d, 24h — mtime での絞り込み
    --format json|tsv|ids

cc-session-finder show <SESSION_ID> [OPTIONS]
  1 セッションの詳細 (タイトル、cwd、mtime、msg_count、first_prompt、
  ファイルパス) を JSON で返す。
  OPTIONS:
    --with-preview N    冒頭 N 件分のユーザーメッセージも本文付きで返す

cc-session-finder index [OPTIONS]
  非対話のインデックス更新コマンド。進捗を stderr に行単位で書き出し、
  完了したら成否を exit code で返す。
  OPTIONS:
    --reindex          DB をクリアして全件再構築
    --no-embed         埋め込み生成をスキップ (メタデータのみ)
    --quiet            進捗を抑制 (エラーのみ stderr)
    --progress json    進捗を JSON Lines で stderr に出力 (機械可読)

cc-session-finder resume <SESSION_ID>
  該当セッションの cwd に chdir して `claude --resume <SESSION_ID>` を exec。
  TUI モードでの Enter と同じ動作。
```

#### JSON 出力スキーマ

`search` / `list` の `--format json` (デフォルト):

```json
{
  "query": "graphql",
  "cwd": "/Users/jugyo/workspace/bringout/seci-cli",
  "results": [
    {
      "session_id": "022d82ca-...",
      "ai_title": "Fix GraphQL query execution in seci-cli",
      "cwd": "/Users/jugyo/workspace/bringout/seci-cli",
      "mtime": "2026-05-19T15:59:00+09:00",
      "msg_count": 87,
      "first_prompt": "<ide_opened_file>...",
      "file_path": "/Users/jugyo/.claude/projects/.../022d82ca-....jsonl",
      "labels": ["cwd", "kw"],
      "scores": {
        "keyword": 12.4,
        "vector": 0.82,
        "recency": 0.95
      }
    }
  ],
  "stats": {
    "total_sessions": 341,
    "keyword_hits": 5,
    "vector_hits": 7,
    "took_ms": 84
  }
}
```

- `tsv` 形式: `session_id\tmtime\tlabels\tai_title` の TAB 区切り。 grep や
  awk と相性良し。
- `ids` 形式: `session_id` のみ 1 行 1 件。 xargs パイプ向け。

#### 標準出力/標準エラーの規約

- 非対話モードでは **stdout は構造化結果のみ**、進捗・ログ・警告は **stderr**。
- 終了コード: `0` = 成功 (結果 0 件でも 0)、`1` = 引数/設定エラー、`2` =
  インデックス不整合、`3` = `show` で session_id が見つからない、`130` =
  `SIGINT` 中断。
- AI エージェントからは `--no-update --format json` を組み合わせれば最速
  (DB の前回索引内容のみで即答)。

#### 自動 CLI モード判定

stdout が TTY でない場合 (パイプ・リダイレクト) は、サブコマンドなしでも
`list --format json` 相当を出力する。`Claude Code` の `Bash` ツール経由で
本ツールが呼ばれた場合に TUI が誤起動するのを防ぐため。

#### 同時実行・ロック

TUI と CLI で同じ SQLite DB を共有する。 SQLite の WAL モード (`journal_mode
= WAL`) を有効化し、reader/writer の同時実行を許容。インデックス書き込みは
1 プロセスのみとし、`flock` で `index.db.lock` を取れなかった場合は CLI
側は既存索引を読むだけにフォールバック (`--no-update` 相当)。

## 段階的実装ステップ

1. **Skeleton** — Cargo init、clap (サブコマンド構造)、ratatui のハロー
   ワールド TUI。
2. **インデックス基盤** — `paths.rs` + `session.rs` + SQLite スキーマ +
   `scan_and_update()`。 `cc-session-finder index` サブコマンドが先に動く。
3. **CLI モード** — `list` / `search (keyword only)` / `show` / `resume` の
   非対話サブコマンドを JSON 出力で先に完成させる。 AI エージェントから
   この時点で利用可能になる (v0.1)。
4. **TUI キーワード経路** — TUI でリアルタイムキーワード絞り込み、cwd /
   mtime ブースト、ラベル付き行表示。 Enter で `resume` 経路を再利用。
5. **埋め込み + ベクトル検索** — fastembed のモデルダウンロード、`sessions_vec`
   への upsert、`vec0` KNN。 `search --mode vector` と TUI 下段マージ表示の
   両方で活用。
6. **インデックス進行中の UI** — `IndexProgress` の watch チャネル、`Scanning`
   ブロック表示、`Embedding` 非ブロックスピナー、CLI `index --progress json`。
7. **磨き込み** — ラベル彩色、IME/マルチバイト境界の入力編集、Unicode 幅、
   端末リサイズ、エラー表示、ログ、WAL / flock。
8. **ドキュメント** — 本計画を `docs/plan.md` にコピー、`README.md` (TUI と
   CLI 両方の使用例)、`cargo install` 手順。

## 検証手順

end-to-end 動作確認 (TUI):
1. `cargo run -- --reindex` で全インデックス構築。`~/.cache/cc-session-finder/
   index.db` を `sqlite3` で開いて `SELECT count(*) FROM sessions;` が 341
   になることを確認。
2. `cc-session-finder` を引数なしで起動: 1 行目が `cc-session-finder` 自身
   のセッション (`[cwd]` ラベル付き、最新)、以降が新しい順であること。
3. **インデックス更新中の挙動** — 一度 DB を消した状態で起動し、`Scanning`
   インジケータと進捗バーが表示されること、その間キー入力 (文字、矢印、
   Enter) が無視されること、`Esc` だけは中断終了になることを確認。
   引き続き `Embedding` フェーズに入ったら検索ボックスが操作可能になり、
   右端にバッチ進捗のスピナーが残ること、完了で消えることを確認。
4. 英語キーワード (例: `graphql`) を入力 → 上部に `[kw]` 行が即座に並ぶ。
5. 日本語キーワード (例: `投資戦略`) を入力 → trigram で日本語タイトルにヒット。
6. 抽象クエリ (例: `database migration safety`) を入力 → 直接の語が含まれない
   セッションも下部に `[~]` 付きで遅れて出る。
7. 適当な行で `Enter` → 該当の cwd で `claude --resume` が起動し、
   セッションが復元されること。
8. ターミナルリサイズで一覧行数が追随することを確認。

end-to-end 動作確認 (CLI):

9. `cc-session-finder list --limit 5 | jq` で JSON が valid であり、5 件、
   `mtime` 降順であること。
10. `cc-session-finder search 'graphql' --format ids` で session_id が改行
    区切りで出ること。
11. `cc-session-finder search 'データベース' --mode vector --format json` で
    語が直接含まれないセッションも `vector` ラベル付きで取れること。
12. `cc-session-finder list | cat` (パイプ経由) で TUI が起動せず JSON が
    出ること (TTY 自動判定)。
13. `cc-session-finder show <存在しない id>` の exit code が `3`、stderr に
    エラーメッセージが出ること。
14. `cc-session-finder index --reindex --progress json 2>progress.log` で
    `progress.log` に JSON Lines が並ぶこと。
15. `cc-session-finder resume <id>` で TUI を介さずに直接 `claude --resume`
    が起動すること。

## 注意・先送り事項

- 巨大セッション (>5MB JSONL) は first_prompt 抽出を冒頭 1MB のみで打ち切る。
- セッションが削除された場合: 索引時に存在しない `session_id` は DB から
  削除 (`sessions` & `sessions_vec` の両方)。
- マルチアカウント / `CLAUDE_PROJECTS_DIR` 上書き等の対応は v2 で検討。
- セッションのプレビュー (中身を別ペインに表示) は v2 で検討。
- 並列インデックス (rayon でファイル分割パース) は 1000 件超えた時点で導入。

## docs/ への出力

承認後、ExitPlanMode を抜けた最初のステップとして、本ファイルの内容を
`/Users/jugyo/workspace/jugyo/cc-session-finder/docs/plan.md` に作成する。
