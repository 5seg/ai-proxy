# ai-proxy 設計メモ

状態: 設計中、実装前。決定済みの事項と未決の事項を分けて書く。

## 1. 目的

手元の1台で OpenAI 互換プロキシを動かし、別マシンから接続させる。上流の API キーはプロキシ機の外に出さない。下流向けのキーは許可モデルを1つだけに絞る。どのマシンがどれだけトークンを消費したかを SQLite に記録し、TUI で確認できるようにする。

## 2. 決定済み

| 項目 | 決定 |
| --- | --- |
| 上流 | command-code(OpenAI 互換)。base URL と API キーは設定ファイル |
| 許可モデル | `deepseek/deepseek-v4.1-flash` のみ |
| `/v1/models` | 制限付きキーには上記1件。`--all-models` キーには上流の `/models` を中継して返す |
| 対象エンドポイント | `/v1/chat/completions`、`/v1/responses` のパススルー |
| 認証 | 下流は `Authorization: Bearer <proxy key>`。キーは端末ごとに1本 |
| 強制 | モデル許可(403)とキーごとの月次トークン上限(429) |
| 本文保存 | 既定は保存しない。指紋とメタだけ。キー単位のデバッグキャプチャは任意 |
| 保持 | リクエスト行は30日、日次ロールアップは永久 |
| 設定リロード | 再起動のみ |
| IPv6 | 対象外。IPv4 のみで待ち受ける |
| client_ip | 保持する。障害調査で参照する |
| `user` フィールド | 改変せずパススルーする |
| 金額 | USD で表示する。ccusage の日次表に合わせる |
| バイナリサイズ | 数値目標を置かない。リリースビルドのみ最適化する |

`deepseek/deepseek-v4.1-flash-fast` は別のモデルIDであり、許可対象に含まない。commandcode CLI の既定モデルはこちらなので、CLI をこのプロキシ経由にすると 403 になる。必要であれば許可リストに追加する。

## 3. 構成

1バイナリにサブコマンドを置く。

```
ai-proxy serve           # プロキシ本体
ai-proxy tui             # 閲覧 TUI
ai-proxy report          # テキスト集計(--csv あり)
ai-proxy config          # 実効設定と、その出所(env/file/default)
ai-proxy key add|list|update|revoke|capture
ai-proxy db prune        # 保持期間を超えた行を削除
ai-proxy db rollup       # 日次集計を作る
```

`serve` と `tui` は同じ DB を開く。書き込みは `serve` だけが行い、`tui` と `report` は SELECT しか発行しない。SQLite は WAL モードにして `busy_timeout` を設定する。

## 3.1 設定

設定ファイルは TOML で、値はすべて `AI_PROXY_*` 環境変数で上書きできる。設定ファイル自体を置かなくても、既定値と環境変数だけで動く(systemd 向け)。

| 設定 | 環境変数 | 既定値 |
| --- | --- | --- |
| `listen` | `AI_PROXY_LISTEN` | `0.0.0.0:8787` |
| `db_path` | `AI_PROXY_DB_PATH` | `$HOME/.local/share/ai-proxy/usage.db` |
| `upstream_base_url` | `AI_PROXY_UPSTREAM_BASE_URL` | `https://api.commandcode.ai/provider/v1` |
| `upstream_api_key` | `AI_PROXY_UPSTREAM_API_KEY` | 空(`serve` では必須) |
| `models` | `AI_PROXY_MODELS` | `deepseek/deepseek-v4.1-flash` |
| `retention_days` | `AI_PROXY_RETENTION_DAYS` | 30 |
| `capture_ttl_hours` | `AI_PROXY_CAPTURE_TTL_HOURS` | 24 |
| `capture_max_bytes` | `AI_PROXY_CAPTURE_MAX_BYTES` | 65536 |
| `max_body_bytes` | `AI_PROXY_MAX_BODY_BYTES` | 67108864 |
| `connect_timeout_secs` | `AI_PROXY_CONNECT_TIMEOUT_SECS` | 10 |
| `read_timeout_secs` | `AI_PROXY_READ_TIMEOUT_SECS` | 300 |

環境変数はファイルより優先する。`AI_PROXY_MODELS` は許可リストを丸ごと置換する。どの値がどこから来たかは `ai-proxy config` が出す。

## 4. 認証とキー

- キーは `sk-aip-` + 32バイトの乱数(base32)で構成し、発行時に一度だけ全体を表示する。
- DB には BLAKE3 ハッシュと先頭8文字の prefix だけを保存する。
- キーは `device` に紐付く。端末の識別はキーで行い、IP は属性として記録するだけにする。IP は DHCP や VPN で変わるため、集計軸には使わない。
- 失効は `revoked_at` を立てる。過去の消費量を保持するため、行は削除しない。
- 許可外モデルへのリクエストには 403 を返す。エラーボディは OpenAI 形式の `{"error": {...}}` にそろえる。

## 5. トークン計測

上流が返す usage をそのまま使う。独自のトークナイズはしない。

ストリーミングでも、上流は `stream_options.include_usage` を付けずに最終チャンクへ `usage` を入れで返す(実測)。よってリクエストボディは書き換えず、チャンクも除去しない。応答を中継しながら最終チャンクの `usage` を読むだけでよい。usage が欠けた場合だけ `usage_source = unknown` とする。

上流の `prompt_tokens_details` は次のように写す。プロバイダによって埋まるキーが違うので、両方を見る。

```
cached_input    = cached_tokens   ?? cache_read_input_tokens   ?? 0
cache_write     = cache_write_tokens ?? cache_creation_input_tokens ?? 0
reasoning       = completion_tokens_details.reasoning_tokens ?? 0
```

`/v1/responses` はフィールド名が違う。単価は同じ扱いでよい。

```
input     = input_tokens            (chat の prompt_tokens に相当)
output    = output_tokens           (chat の completion_tokens に相当)
cached    = input_tokens_details.cached_tokens
reasoning = output_tokens_details.reasoning_tokens
```

ストリーミング時の入れ子も違う。chat は `data` 直下の `usage`、responses は最終イベントの `response.usage` に入る。どちらも同じ抽出器で読む。

responses は上流の既定でキャッシュが無効(`"caching":{"type":"disabled"}`)なので、`cached_input` は 0 になる。

内部表現は次の5つに固定する。

```
input_tokens          通常の入力
cached_input_tokens   上流のプロンプトキャッシュから読んだ入力
cache_write_tokens    キャッシュへの書き込み
output_tokens         出力
reasoning_tokens      うち推論分(output に含まれる実装があるので二重計上しない)
```

`usage_source` は `upstream` / `unknown` の2値とし、推定値は入れない。不明率が高いと数字の意味が変わるため、TUI に不明率を表示する。

キャッシュヒット率は次の式で固定する。

```
rate = cached_input / (input + cached_input + cache_write)
```

分母にキャッシュ書き込みを含めるのは、Anthropic 系のプロバイダが `input_tokens` にキャッシュ分を含めないためである。この定義を TUI の画面に1行で表示する。command-code では `prompt_tokens` がキャッシュ分を含むことを実測で確認済みで、コスト計算では差し引く。

## 4.1 キーの種類

キーは2種類ある。既定は制限付き。

- 制限付き: 許可モデル以外は 403、月次上限を超えると 429。
- 無制限: `--all-models` でモデル許可リストを外し、`--limit` を付けなければ上限も無い。信頼する開発機に渡すキー向け。

上限は million 単位で書ける。`--limit 100M` は 100,000,000。接尾辞は `k`(千)・`m`(百万)・`b`/`g`(十億)、`_` 区切りも受け付ける。`--limit unlimited` で上限を外す。

```bash
ai-proxy key add --device laptop --limit 100M        # 制限付き、月1億トークン
ai-proxy key add --device workstation --all-models   # モデル制限なし、上限なし
ai-proxy key update 3MS4P2J3 --limit unlimited       # 後から外す
ai-proxy key update 3MS4P2J3 --all-models --limit 500M
```

無制限キーは `flash-fast` も通す。「flash-fast を許可しない」は許可リストの運用で守る。無制限キーにも効く拒否リストは実装していない。

`/v1/models` の応答もキーで変わる。制限付きキーには許可リストを合成して返し、`--all-models` キーには上流の `/models` の本文をそのまま返す(96 モデル、`context_length` や `supported_endpoints` も含む)。キーの制限を一覧に出しても意味がないためである。上流の応答は `models_ttl_secs`(既定 300 秒)キャッシュし、取得に失敗したら許可リストに退避して警告を出す。

`/v1/models/{id}` は制限付きキーで許可外なら 404、`--all-models` キーなら任意の id に 200。

`allow_all_models` は schema v2 で追加した。既存 DB は起動時に `ALTER TABLE` で移行し、既存キーは制限付きになる。

## 6. クォータ

キーごとに 3 つの窓でトークン上限を持つ。`monthly_token_limit` / `weekly_token_limit` /
`five_hour_token_limit` がすべて NULL なら上限なしで、判定自体を飛ばす。判定は受付時と完了時の2回行う。

| 窓 | 列 | 範囲 |
| --- | --- | --- |
| 月次 | `monthly_token_limit` | 暦月(UTC 1日 00:00 から) |
| 週次 | `weekly_token_limit` | 直近 7 日間のローリング |
| 5 時間 | `five_hour_token_limit` | 直近 5 時間のローリング |

- 受付時: 各窓の合計が上限以上であれば 429 を返す。窓は 5 時間 → 週次 → 月次の順に見て、
  最初に当たった窓の名前を `error.message` に出す(最も詰まっている窓を返すため)。
- `Retry-After`: 月次は月初まで。ローリング窓は `MIN(started_at) + 窓長 - now`、
  つまり窓の中で一番古いリクエストが外れるまで。窓が空なら窓長を返す。
- 実行中: 打ち切らない。最後まで流し、次回の受付で拒否する。打ち切ると usage が取れず、クライアントには中途半端な応答が渡るためである。
- 完了時: 使用量を加算し、`quota_exceeded` を立てる。判定には複数窓のうち残量が最小のものを使う。

合計は `request` テーブルの `SUM` で求める。`(api_key_id, started_at)` の索引があれば数千行の走査で済む。
3 窓でもクエリは `since` が違うだけで、索引の範囲走査が 3 回になるだけ。同時多発リクエストで
上限をわずかに超えることは許容し、厳密な予約はしない。

月次だけ暦月で、短い 2 窓をローリングにしたのは、短い窓の目的が「暴走の停止」だからである。
固定ブロック(例: epoch から 5 時間区切り)だと境界を跨いで 2 倍使え、目的に反する。
暦月は請求の区切りとして人間が読む単位なのでそのままにした。

上限判定に使うトークン数は、まず `input + output` の生の合計とする。キャッシュ読み取りの重み付けを変えたいときに備えて、重みは設定値にしておく(`quota_weight_*` は未使用)。

## 7. 保存

```sql
device(id, name, note, created_at)
api_key(id, device_id, key_hash, prefix, created_at, revoked_at, monthly_token_limit,
        weekly_token_limit, five_hour_token_limit,
        allow_all_models, quota_weight_input, quota_weight_cached, quota_weight_output,
        capture_bodies, capture_until)
request(id, ts, device_id, api_key_id, client_ip, endpoint, model, stream,
        status_code, latency_ms, ttft_ms,
        input_tokens, cached_input_tokens, cache_write_tokens,
        output_tokens, reasoning_tokens, usage_source,
        prompt_hash, message_count, prompt_chars, has_tools, has_image,
        temperature, max_tokens, aborted, quota_exceeded, error_kind)
price(model, effective_from, input_per_mtok, cached_per_mtok, cache_write_per_mtok, output_per_mtok)
daily_rollup(day, device_id, model, requests, input_tokens, cached_input_tokens,
             cache_write_tokens, output_tokens, reasoning_tokens)
debug_capture(request_id, api_key_id, expires_at, prompt_body, response_body)
```

- `request` は受付時に `in_progress` 相当の行を作り、完了時に更新する。クラッシュ時の取りこぼし防止と、実行中リクエストの TUI 表示の両方を満たすためである。
- `prompt_hash` は先頭4KBの BLAKE3。`message_count`、`prompt_chars`、`has_tools`、`has_image` と合わせて、コスト差の説明に使う。
- 本文は既定で保存しない。`debug_capture` はキー単位で有効化し、`capture_ttl_hours`(既定24時間)で消す。1件 `capture_max_bytes`(既定64KB)で切り、切ったら `truncated=1`。プロンプトだけでは原因が分からないことが多いため、応答本文も一緒に保存する。ストリーミングの応答は流したバイト列を連結したもので、SSE のまま入る。
- キーの `capture_until` は本文保存の有効期限である。期限を過ぎたキーは `capture_bodies=0` に戻す(`db prune` と起動時)。
- `price` は計算済みの金額を保存しないための表であり、金額はクエリ時に再計算する。
- 30日より古い `request` 行は `db prune` で削除する。`daily_rollup` は残す。
- DELETE してもファイルは縮まないため、`auto_vacuum = INCREMENTAL` にして `db prune` のあとに `incremental_vacuum` を実行する。

## 8. TUI

床面専用で、書き込みはしない。画面は次の5つ。

- 概要: 合計(リクエスト数、入力、キャッシュヒット率、出力、金額、キャッシュ節約額)と日次表
- 端末: 端末ごとのトークン数、リクエスト数、キャッシュヒット率、金額
- IP: client_ip ごとの同じ列
- モデル: モデルごとの内訳
- キー: キー prefix ごとの内訳

列は共通で `reqs / input / cached / hit / output / cost(USD)`、最終行に合計を置く。期間は `retention_days`。

更新は2秒間隔の取り直しで行う。キー操作は `q` 終了、`1`〜`5` または `Tab` で画面切替、`j`/`k` と `PageUp`/`PageDown` でスクロール、`r` で再読み込み。端末の復元は Drop で行い、エラーは画面内に出してパニックさせない。

## 9. バイナリと依存

- 依存: tokio、hyper または axum、serde_json、rustls(ring、webpki-roots)、rusqlite(bundled)、ratatui + crossterm、blake3
- musl 静的リンク。`strip`、`lto = "fat"`、`codegen-units = 1`、`opt-level = "s"`、`panic = "abort"`
- 見込みは 5〜7MB。数値目標を置くなら先に決める。
- 上流が HTTPS なので TLS クライアントは必須。下流側の TLS は前段に任せ、プロキシ自体は平文 HTTP で待ち受ける。
- SSE はバッファせずチャンク単位でフラッシュする。usage チャンクを落とす処理だけ例外とする。

## 10. 決定の補足

- 許可モデルは `deepseek/deepseek-v4.1-flash` の1件に固定し、`-fast` は許可しない。
- 金額は `price` 表から都度計算する。単価は commandcode のカタログ値(input 0.15、cache read 0.003、output 0.60 USD / 100万トークン)を使う。
- デバッグキャプチャの既定 TTL は24時間とし、キー単位で変更できるようにする。

未決の項目は残っていない。実装中に実測で確定させる確認点は `PLAN.md` の8章に置いた。

## 11. 実装順序

1. 設定の読み込みと `/healthz`、`/v1/models` — 完了
2. `/v1/chat/completions` のパススルーと usage 記録(非ストリーム) — 完了
3. ストリーミングのパススルーと usage チャンクの除去 — 完了
4. `/v1/responses` の対応 — 完了
5. キー発行 CLI とモデル制限 — 完了
6. クォータ判定 — 完了
7. 集計(`report`)と TUI — 完了
8. 保持と prune、日次ロールアップ — 完了
9. デバッグキャプチャ — 完了
10. 実機投入 — unit は同梱しない(設定は環境変数だけで完結する)。Alpine の OpenRC で稼働中。
    下流マシン2台での運用確認は未了
