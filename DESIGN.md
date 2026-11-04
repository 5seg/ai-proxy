# ai-proxy 設計メモ

状態: 設計中、実装前。決定済みの事項と未決の事項を分けて書く。

## 1. 目的

手元の1台で OpenAI 互換プロキシを動かし、別マシンから接続させる。上流の API キーはプロキシ機の外に出さない。下流向けのキーは許可モデルを1つだけに絞る。どのマシンがどれだけトークンを消費したかを SQLite に記録し、TUI で確認できるようにする。

## 2. 決定済み

| 項目 | 決定 |
| --- | --- |
| 上流 | command-code(OpenAI 互換)。base URL と API キーは設定ファイル |
| 許可モデル | `deepseek/deepseek-v4.1-flash` のみ |
| `/v1/models` | 上記1件だけを返す |
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
ai-proxy key add|list|revoke
ai-proxy db prune        # 30日より古い行を削除
ai-proxy export [--csv]
```

`serve` と `tui` は同じ DB を開く。書き込みは `serve` だけが行い、`tui` は読み取り専用で開く。SQLite は WAL モードにして `busy_timeout` を設定する。

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

## 6. クォータ

キーごとに月次トークン上限を持つ。判定は受付時と完了時の2回行う。

- 受付時: 当月合計が上限以上であれば 429 を返す。`Retry-After` は月初まで。
- 実行中: 打ち切らない。最後まで流し、次回の受付で拒否する。打ち切ると usage が取れず、クライアントには中途半端な応答が渡るためである。
- 完了時: 使用量を加算し、`quota_exceeded` を立てる。

当月合計は `request` テーブルの `SUM` で求める。`(api_key_id, ts)` の索引があれば数千行の走査で済む。同時多発リクエストで上限をわずかに超えることは許容し、厳密な予約はしない。

上限判定に使うトークン数は、まず `input + output` の生の合計とする。キャッシュ読み取りの重み付けを変えたいときに備えて、重みは設定値にしておく。

## 7. 保存

```sql
device(id, name, note, created_at)
api_key(id, device_id, key_hash, prefix, created_at, revoked_at, monthly_token_limit)
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
- 本文は既定で保存しない。`debug_capture` はキー単位で有効化し、24時間で消す。1件64KBで切る。プロンプトだけでは原因が分からないことが多いため、応答本文も一緒に保存する。
- `price` は計算済みの金額を保存しないための表であり、金額はクエリ時に再計算する。
- 30日より古い `request` 行は `db prune` で削除する。`daily_rollup` は残す。
- DELETE してもファイルは縮まないため、`auto_vacuum = INCREMENTAL` にして `db prune` のあとに `incremental_vacuum` を実行する。

## 8. TUI

閲覧専用で、書き込みはしない。画面は次の5つ。

- 端末×期間: 端末ごとのトークン数、リクエスト数、キャッシュヒット率
- モデル別: モデルごとの内訳
- 日次推移: 日ごとの棒グラフ
- 上位リクエスト: 消費の大きいリクエスト
- 状態: usage 不明率、エラー率、最終書き込み時刻

更新は1秒間隔のポーリングで行う。キー操作は期間の切り替え(今日/7日/30日)とソートのみ。

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

1. 設定の読み込みと `/healthz`、`/v1/models`
2. `/v1/chat/completions` のパススルーと usage 記録(非ストリーム)
3. ストリーミングのパススルーと usage チャンクの除去
4. `/v1/responses` の対応
5. キー発行 CLI とモデル制限
6. クォータ判定
7. TUI
8. 保持と prune
9. デバッグキャプチャ
