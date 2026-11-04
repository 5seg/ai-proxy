# ai-proxy

OpenAI 互換のプロキシ。許可したモデルだけを通し、端末ごとのトークン消費を SQLite に記録する。
計測の芯は「リクエスト1本 = 1行」で、上流が返した usage をそのまま保存する。金額は保存せず、
問い合わせのたびに `price` 表から再計算する。

```
client (OpenAI SDK) → ai-proxy → 上流
                        └→ usage.db (request, daily_rollup, debug_capture)
```

## ビルド

```bash
cargo build --release          # target/release/ai-proxy
cargo bloat --release --crates # サイズの内訳を見たいとき
```

`Cargo.toml` の release プロファイルは `opt-level="s"`、LTO、`strip`、`panic=abort`。
SQLite を同梱しても **4.9 MB**。

このマシンのホストは `x86_64-alpine-linux-musl` なので、既定の `cargo build --release` が
そのまま musl バイナリになる(`ldd` は `libc.musl-x86_64.so.1` と `libgcc_s` を出す)。

完全静的バイナリはこのホストでは作れない。`RUSTFLAGS='-C target-feature=+crt-static'` は
`clap_derive` などの proc-macro クレートで `does not support these crate types` になって落ちる。
rustup も入っておらず、`x86_64-unknown-linux-musl` ターゲットを追加できない。
静的バイナリが要るときは、rustup のあるマシンで `rustup target add x86_64-unknown-linux-musl`
してから `cargo build --release --target x86_64-unknown-linux-musl` する。

## 起動

```bash
ai-proxy serve --config ./ai-proxy.toml
```

設定ファイルは省略できる。その場合の実効値は既定値と `AI_PROXY_*` 環境変数で決まる。
上流キーが空だと `serve` は起動を拒否する。

## 設定

設定の実効値と、それぞれがどこから来たか(env / file / default)を確認できる。

```bash
ai-proxy config --config ./ai-proxy.toml
```

| 設定 | 環境変数 | 既定値 |
| --- | --- | --- |
| `listen` | `AI_PROXY_LISTEN` | `0.0.0.0:8787` |
| `db_path` | `AI_PROXY_DB_PATH` | `$HOME/.local/share/ai-proxy/usage.db` |
| `upstream_base_url` | `AI_PROXY_UPSTREAM_BASE_URL` | `https://api.commandcode.ai/provider/v1` |
| `upstream_api_key` | `AI_PROXY_UPSTREAM_API_KEY` | (空。`serve` では必須) |
| `models` | `AI_PROXY_MODELS` (カンマ区切り) | `deepseek/deepseek-v4.1-flash` |
| `retention_days` | `AI_PROXY_RETENTION_DAYS` | `30` |
| `capture_ttl_hours` | `AI_PROXY_CAPTURE_TTL_HOURS` | `24` |
| `capture_max_bytes` | `AI_PROXY_CAPTURE_MAX_BYTES` | `65536` |
| `max_body_bytes` | `AI_PROXY_MAX_BODY_BYTES` | `67108864` (64 MiB) |
| `connect_timeout_secs` | `AI_PROXY_CONNECT_TIMEOUT_SECS` | `10` |
| `read_timeout_secs` | `AI_PROXY_READ_TIMEOUT_SECS` | `300` |

環境変数は設定ファイルより優先する。`AI_PROXY_MODELS` を設定すると許可モデルが丸ごと置き換わる。
`RUST_LOG` はそのまま効く(`RUST_LOG=ai_proxy=debug`)。

`models` に無いモデルは 403 `model_not_allowed` で拒否する。`deepseek-v4.1-flash-fast` は
許可リストに入れない方針なので、既定にも含めていない。`--all-models` のキーだけはこの判定を
通らない(`/v1/models/{id}` も任意の id に 200 を返す)。

## キー

```bash
# 発行。キー全文はこのときしか出ない
ai-proxy key add --device laptop --limit 5M      # 月500万トークンまで(モデルは許可リスト)
ai-proxy key add --device ws --all-models        # モデル制限なし、上限なし
# 本文保存を有効にして発行(既定 24 時間で自動オフ)
ai-proxy key add --device laptop --capture

ai-proxy key list
ai-proxy key revoke 3MS4P2J3
ai-proxy key capture 3MS4P2J3 --hours 2   # 既存キーの本文保存を後から有効化
ai-proxy key capture 3MS4P2J3 --off
ai-proxy key update 3MS4P2J3 --limit 20M  # 上限を後から変える
ai-proxy key update 3MS4P2J3 --limit unlimited --all-models
```

`--limit` は million 単位で書ける。`100M` = 100,000,000、ほかに `k` / `b`(`g`)、`_` 区切り。
`--limit` を省くか `unlimited` を渡すと上限なし(既定)。`--all-models` を付けたキーは
許可モデルの判定を通らない。

上限に達したキーは 429 `insufficient_quota` と `Retry-After` を返し、次の月初まで通り抜けない。
上限を跨いだリクエストは最後まで流し、`request.quota_exceeded` に 1 を立てる。

キーは `Authorization: Bearer sk-aip-...`、`x-api-key`、`api-key` のどれでも受け付ける。

## 集計と表示

```bash
ai-proxy report                    # 直近30日の日次・端末・IP・モデル・キー
ai-proxy report --days 7
ai-proxy report --month 2026-11
ai-proxy report --csv > usage.csv
ai-proxy tui                       # 端末UI。q で終了、1-5 または Tab で画面切替
```

cache hit 率は `cached_input / input`。入力は上流が返した `prompt_tokens`(`input_tokens`)を
そのまま保存しているので、キャッシュ分を含む。コストは差し引いて計算する。
```
cost = (input - cached) * input_per_mtok
     + cached          * cached_per_mtok
     + cache_write     * cache_write_per_mtok
     + output          * output_per_mtok      (すべて USD / 100万トークン)
```

単価は `price` 表(`model`, `effective_from`, 4種の単価)。値上げしたら行を追加する。

```sql
INSERT INTO price VALUES ('deepseek/deepseek-v4.1-flash', '2026-12-01', 0.20, 0.004, 0.0, 0.80);
```

単価の無いモデルを通したときは、その分を 0 として足さず、コストに `*` を付けて
「下限値である」と表示する。`report` は単価の無いモデル名を並べ、`csv` は `unpriced` 列に出す。
`--all-models` のキーを使うと起きるので、モデルを増やしたら価格行も足す。

## 保存期間と削除

`request` は `retention_days`(既定30日)より古い行を起動時に削除する。`daily_rollup` は残すので、
長期の傾向は `ai-proxy db rollup --days 365` で日次集計を作っておけば prune 後も追える。

```bash
ai-proxy db prune            # 設定の保持期間で削除 + incremental_vacuum
ai-proxy db prune --days 60  # 期間を一時的に変える
ai-proxy db rollup --days 365
ai-proxy db rollup && ai-proxy db prune   # 集計してから消す
```

## 本文の保存

既定では本文を保存しない。`prompt_hash`(先頭4KBの BLAKE3)と `prompt_chars`、`has_tools`、
`has_image` だけを残す。プロンプトと応答の本文が要るときだけ、キー単位で `--capture` を有効にする。

- 保存先は `debug_capture`。`capture_ttl_hours` で期限を切り、`db prune` と起動時に消す。
- 1件 `capture_max_bytes` で切り、切ったら `truncated=1`。
- ストリーミングの応答は流したバイト列を連結して保存する(SSE のまま)。

## systemd

```bash
sudo install -m755 target/release/ai-proxy /usr/local/bin/ai-proxy
sudo install -d -m750 /etc/ai-proxy
sudo cp deploy/ai-proxy.service /etc/systemd/system/
sudo cp deploy/env.example /etc/ai-proxy/env && sudo chmod 600 /etc/ai-proxy/env
$EDITOR /etc/ai-proxy/env          # AI_PROXY_UPSTREAM_API_KEY を書く
sudo systemctl daemon-reload && sudo systemctl enable --now ai-proxy
journalctl -u ai-proxy -f
```

unit は `StateDirectory=ai-proxy` を使い、DB を `/var/lib/ai-proxy/usage.db` に置く。
`--config` を渡さないので、設定はすべて環境変数から来る。

## エンドポイント

| パス | 用途 |
| --- | --- |
| `GET /healthz` | 認証不要の生存確認。DB に1クエリ投げる |
| `GET /v1/models` | 許可モデルの一覧 |
| `GET /v1/models/{id}` | 許可モデル1件 |
| `POST /v1/chat/completions` | 中継(ストリーム対応) |
| `POST /v1/responses` | 中継(ストリーム対応) |

`/v1/*` はすべてキー必須。上流には `Authorization` を付け替えて転送し、`user` フィールドは改変しない。
