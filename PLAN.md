# ai-proxy 実装プラン

DESIGN.md の決定を前提に、作業を分割したもの。金額と料金表は commandcode のカタログ実測値に基づく。

## 1. 確定事項(今回の追加分を含む)

| 項目 | 決定 |
| --- | --- |
| 上流 | `https://api.commandcode.ai/provider/v1`(OpenAI 互換) |
| 許可モデル | `deepseek/deepseek-v4.1-flash` のみ。`-fast` は不許可 |
| 対応 API | `/chat/completions`、`/responses`(上流は両方に対応) |
| `/v1/models` | 1件だけ返す |
| client_ip | 残す。調査用に TUI と report から見える |
| `user` フィールド | 無改変でパススルー |
| 金額 | USD で表示。ccusage の日次表に合わせる |
| バイナリサイズ | 数値目標なし。リリースビルドは最適化する |
| 保持 | request 30日、daily_rollup 永久 |
| リロード | 再起動のみ |
| IPv6 | 対象外 |

## 2. 料金

カタログ実測値(USD / 100万トークン)。

| 対象 | input | cache read | cache write | output |
| --- | --- | --- | --- | --- |
| deepseek/deepseek-v4.1-flash | 0.15 | 0.003 | 0(項目なし) | 0.60 |

```rust
// OpenAI 系の慣例では prompt_tokens が cached_tokens を含むため、input から差し引く
cost = ((input - cached) * 0.15 + cached * 0.003 + output * 0.60) / 1_000_000.0;
```

`input < cached` になった場合は差し引きをやめ、`input * 0.15 + cached * 0.003` に切り替える(フェーズ2の実測で確定)。

## 3. 技術スタック

| 用途 | crate | 備考 |
| --- | --- | --- |
| 非同期実行 | tokio | rt-multi-thread, net, signal |
| HTTP サーバ | axum 0.8 | ルーティングとストリーム応答 |
| HTTP クライアント | reqwest(default-features=false) | rustls-tls, stream。上流は HTTP/1.1 で足りる |
| JSON | serde, serde_json | |
| DB | rusqlite(bundled) | 静的リンクのため同梱 |
| TUI | ratatui, crossterm | |
| ハッシュ | blake3 | キーとプロンプト指紋 |
| 乱数 | getrandom | キー生成 |
| エンコード | data-encoding | base32(Crockford、パディングなし) |
| 設定 | toml, serde | |
| CLI | clap(derive) | サブコマンド5個。手書きにすれば数百KB減るが保守性を取る |
| 時刻 | time | 日付境界の計算 |
| ログ | tracing, tracing-subscriber | env-filter |

release プロファイル: `strip = true`, `lto = "fat"`, `codegen-units = 1`, `opt-level = "s"`, `panic = "abort"`。musl ターゲットで静的リンク。

## 4. DDL

接続時に必ず設定する。

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
```

`auto_vacuum = INCREMENTAL` は DB 作成時のみ有効なので、初期化時に設定してから `VACUUM` する。

```sql
user_version = 1

CREATE TABLE device (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,     -- macbook, studio など
  note        TEXT,
  created_at  INTEGER NOT NULL          -- unix ms
);

CREATE TABLE api_key (
  id                 INTEGER PRIMARY KEY,
  device_id          INTEGER NOT NULL REFERENCES device(id),
  key_hash           BLOB NOT NULL UNIQUE,   -- blake3(全体キー), 32bytes
  prefix             TEXT NOT NULL,          -- 表示用 先頭8文字
  created_at         INTEGER NOT NULL,
  revoked_at         INTEGER,
  monthly_token_limit INTEGER,               -- NULL = 無制限
  quota_weight_input  REAL NOT NULL DEFAULT 1.0,
  quota_weight_cached REAL NOT NULL DEFAULT 1.0,
  quota_weight_output REAL NOT NULL DEFAULT 1.0,
  capture_bodies     INTEGER NOT NULL DEFAULT 0,
  capture_until      INTEGER
);

CREATE TABLE request (
  id                 INTEGER PRIMARY KEY,
  started_at         INTEGER NOT NULL,
  finished_at        INTEGER,
  device_id          INTEGER NOT NULL REFERENCES device(id),
  api_key_id         INTEGER NOT NULL REFERENCES api_key(id),
  client_ip          TEXT NOT NULL,          -- IPv4 文字列。調査用に保持
  endpoint           TEXT NOT NULL,          -- chat_completions | responses
  model              TEXT NOT NULL,
  stream             INTEGER NOT NULL,
  status_code        INTEGER,
  error_kind         TEXT,                   -- upstream_4xx|upstream_5xx|model_denied|quota|client_abort|proxy
  latency_ms         INTEGER,
  ttft_ms            INTEGER,
  input_tokens       INTEGER,
  cached_input_tokens INTEGER,
  cache_write_tokens  INTEGER,
  output_tokens      INTEGER,
  reasoning_tokens   INTEGER,
  usage_source       TEXT NOT NULL,          -- upstream | unknown
  prompt_hash        BLOB,                   -- 先頭4KBの blake3
  message_count      INTEGER,
  prompt_chars       INTEGER,
  has_tools          INTEGER,
  has_image          INTEGER,
  temperature        REAL,
  max_tokens         INTEGER,
  aborted            INTEGER NOT NULL DEFAULT 0,
  quota_exceeded     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_request_started  ON request(started_at);
CREATE INDEX idx_request_device   ON request(device_id, started_at);
CREATE INDEX idx_request_key      ON request(api_key_id, started_at);
CREATE INDEX idx_request_hash     ON request(prompt_hash);

CREATE TABLE price (
  model                   TEXT NOT NULL,
  effective_from          TEXT NOT NULL,      -- YYYY-MM-DD
  input_per_mtok          REAL NOT NULL,
  cached_per_mtok         REAL NOT NULL,
  cache_write_per_mtok    REAL NOT NULL,
  output_per_mtok         REAL NOT NULL,
  PRIMARY KEY (model, effective_from)
);

CREATE TABLE daily_rollup (
  day                TEXT NOT NULL,          -- ローカル日付 YYYY-MM-DD
  device_id          INTEGER NOT NULL,
  model              TEXT NOT NULL,
  requests           INTEGER NOT NULL,
  input_tokens       INTEGER NOT NULL,
  cached_input_tokens INTEGER NOT NULL,
  cache_write_tokens  INTEGER NOT NULL,
  output_tokens      INTEGER NOT NULL,
  reasoning_tokens   INTEGER NOT NULL,
  PRIMARY KEY (day, device_id, model)
);

CREATE TABLE debug_capture (
  request_id   INTEGER PRIMARY KEY REFERENCES request(id),
  api_key_id   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  truncated    INTEGER NOT NULL,
  prompt_body  TEXT,
  response_body TEXT
);
```

既存 DB の参照は `(device_id, started_at)` と `(api_key_id, started_at)` で足りる。30日で数千〜数十万行を想定しており、索引なしの集計は避ける。

## 5. モジュール構成

```
src/main.rs            サブコマンド分岐
src/config.rs          TOML 読み込み
src/db.rs              接続、PRAGMA、マイグレーション、INSERT/UPDATE、集計クエリ
src/key.rs             キー生成、照合、表示形式
src/proxy/mod.rs       axum ルータ、共通ミドルウェア
src/proxy/auth.rs      Bearer → api_key 行(照合キャッシュ付き)
src/proxy/models.rs    /v1/models、/v1/models/{id}
src/proxy/chat.rs      /v1/chat/completions パススルー
src/proxy/responses.rs /v1/responses パススルー
src/proxy/sse.rs       SSE 再フレーミング、usage チャンク除去
src/proxy/usage.rs     usage 抽出(2形式)、正規化、コスト
src/proxy/quota.rs     月次合計、429 生成
src/report.rs          日次集計、端末別、モデル別、上位
src/tui.rs             ratatui 画面
```

## 6. 実装フェーズ

各フェーズは「動作確認できる状態」で終える。

### フェーズ 0: 骨組み(1〜2時間)
config、tracing、DB 初期化、`/healthz`、clap サブコマンド。
完了: `ai-proxy serve` が起動し `/healthz` が 200、DB が作られ `user_version=1`、`ai-proxy --help` に5サブコマンド。

### フェーズ 1: 認証・モデル制限・`/v1/models`(2〜3時間)
Bearer 照合、失効判定、許可モデルの完全一致判定、`/v1/models` の固定応答。
完了:
- 正しいキー → 200、`data` が1件
- キーなし/不正 → 401、OpenAI 形式のエラーボディ
- 失効キー → 401
- `deepseek/deepseek-v4.1-flash-fast` → 403
- 到達不能なキーで 404 が返らないこと

### フェーズ 2: chat/completions 非ストリーム(3〜4時間)
上流へ転送、ステータス/ヘッダ素通し、usage 記録、`request` 行の開始と完了。
完了:
- 実リクエスト1本で `input_tokens`/`output_tokens` が入る
- `prompt_tokens` に cached が含まれるかを実測で確定し、コスト式を固定
- 同じプロンプトを `ccusage-commandcode-sync` で集計した数字と突き合わせ、差の理由を説明できる
- 400/500 系は `error_kind` 付きで記録され、クライアントに本文がそのまま渡る

### フェーズ 3: ストリーミング(4〜5時間)
SSE をチャンク単位で中継、`stream_options.include_usage` の注入、usage チャンクの下流除去、TTFT 計測、切断検知。
完了:
- `curl -N` でトークンが流れる
- 注入した場合も下流には usage チャンクが出ない
- 下流が usage を要求していた場合は素通しされる
- 上流が `stream_options` を受け付けない場合の再送経路が動き、`usage_source=unknown` が記録される
- 途中で Ctrl-C → `aborted=1`、上流接続も切れる
- 30秒以上の生成でタイムアウトしない(総タイムアウトは設定しない。接続と無通信だけ見る)

### フェーズ 4: /v1/responses(2〜3時間)
パススルーと、`response.completed` からの usage 抽出。
完了: responses のストリームで input/output/cached が `request` に入る。`/v1/models` と整合。

### フェーズ 5: クォータ(2〜3時間)
受付時の月次合計判定、429 と `Retry-After`(月初)、完了時の加算と `quota_exceeded`。
完了:
- `monthly_token_limit` を小さくしたキーで2本目が 429
- 同時5リクエストでも二重加算がない(トランザクションで確認)
- 打ち切りは起きない(1本目は最後まで流れる)

### フェーズ 6: キー管理 CLI(2時間)
`key add --device X --limit N [--capture]`、`key list`、`key revoke`。
完了: 発行→即利用→失効→401。発行時にだけ全体が表示され、DB にはハッシュと prefix のみ。

### フェーズ 7: 集計と TUI(5〜6時間)
日次集計、コスト計算、端末別/モデル別/上位/状態の各画面、client_ip の表示。
完了:
- `ai-proxy report` の日次表が ccusage の表と同じ列構成で出る
- 合計が ccusage の同期データと一致する(丸め誤差の範囲)
- TUI が1秒で更新され、`serve` の書き込み中も固まらない
- usage 不明率、エラー率、最終書き込み時刻が出る

### フェーズ 8: 保持と prune(2時間)
`db prune`、日次ロールアップの確定、`incremental_vacuum`。
完了: 31日前の行を仕込み、prune 後に request から消えて rollup に残る。ファイルサイズが縮む。二重実行しても安全。

### フェーズ 9: デバッグキャプチャ(2時間)
キー単位のフラグ、本文と応答の保存、TTL 削除、64KB 上限。
完了: フラグ ON のキーだけ本文が入る。応答はストリームを連結して復元される。24時間後に消える。フラグ OFF のキーでは1行も入らない。

### フェーズ 10: 実機投入(2〜3時間)
systemd unit、Tailscale 経由の bind、下流マシン2台で実運用、初回レポートの突き合わせ。
完了: 2台から利用でき、TUI で端末別の差が見え、ccusage の数字と整合する。

合計 27〜33時間。フェーズ 0〜4 で計測の芯が完成し、そこから先は運用機能。

## 7. 受け入れ確認

実キーを使う確認はフェーズ 2、3、4、7、10 の5回。1回あたり数リクエストに留める。

拒否系はすべて curl で確認する。

```bash
curl -s localhost:8787/healthz
curl -s localhost:8787/v1/models -H "Authorization: Bearer $K"
curl -s -o /dev/null -w '%{http_code}\n' localhost:8787/v1/models
curl -s localhost:8787/v1/chat/completions -H "Authorization: Bearer $K" \
  -H 'content-type: application/json' \
  -d '{"model":"deepseek/deepseek-v4.1-flash-fast","messages":[{"role":"user","content":"hi"}]}'
```

## 8. 実装中に潰す確認点

1. `prompt_tokens` が cached を含むか(コスト式の符号が変わる)
2. 上流が `stream_options.include_usage` を受け付けるか(フォールバック経路の要否)
3. `/v1/responses` のストリームで usage が来るか
4. axum のボディ上限。1M コンテキストを想定すると `DefaultBodyLimit` の既定 2MB では足りない。64MB に上げる
5. 上流が返す `model` 名が要求と一致するか(応答に別名が入ると TUI の集計軸がぶれる)

## 9. 実装しないこと

- 独自トークナイズ(usage 不明は unknown として記録する)
- レスポンスキャッシュ
- 複数上流の切り替え、フォールバック
- レート制限(RPM/TPM)
- SIGHUP リロード
- IPv6
- 金額の DB 保存(常に価格表から再計算する)
