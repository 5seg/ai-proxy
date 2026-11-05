# タスク: opencode に ai-proxy プロバイダを追加する

あなたはコーディングエージェントである。ユーザーの opencode に `ai-proxy` という
プロバイダを 1 つ追加し、そこ経由でモデルが使える状態にする。**既存のプロバイダ設定は
1 文字も変えない。** 追記とマージだけを行う。

## 入力

- **API キー**: `sk-aip-...`。このプロンプトに書かれているか、環境変数 `AIPROXY_KEY`
  にある。どちらも無ければ推測せず、「キーが見つからない」と言って止まる。
- **ベース URL**: 次の 2 択。テールネット内なら上、外なら下。まず上を試し、
  接続できなければ下に切り替える。
  - `http://100.127.113.52:8787/v1`(Tailscale 内のみ)
  - `https://router.ht1.top/v1`(どこからでも)
- **モデル**: 決め打ちしない。下の手順 2 で `GET /v1/models` が返したものだけを書く。

## 手順

### 1. 既存設定を読む

対象は次の順で最初に見つかったもの。

1. `~/.config/opencode/opencode.jsonc`
2. `~/.config/opencode/opencode.json`
3. どちらも無ければ `~/.config/opencode/opencode.json` を新規作成する

`.jsonc` はコメント付き JSON なので、パースする前にコメントを落とすか、コメントを
保ったまま `provider` の中だけを編集する。**どちらの場合も既存の `provider` は
マージ対象であって置換対象ではない。** 他のキー(`$schema`、`instructions`、
`plugin`、`mcp`、`permission` など)には触らない。

### 2. このキーで使えるモデルを確認する

```bash
curl -sS -o /tmp/aiproxy-models.json -w '%{http_code}\n' \
  "$BASE/v1/models" -H "Authorization: Bearer $KEY"
```

- `200`: `/tmp/aiproxy-models.json` の `data[].id` がこのキーで使えるモデル。
- `401`: キーが違うか失効している。止まる。
- 接続失敗: もう一方のベース URL でやり直す。両方駄目なら止まる。

### 3. プロバイダを追記する

`provider` オブジェクトに `ai-proxy` を足す。`<BASE>` は手順 2 で通った URL、
`<KEY>` はキー、`<MODEL_ID>` は手順 2 で得た ID(複数あれば全部)に置き換える。

```jsonc
"ai-proxy": {
  "npm": "@ai-sdk/openai-compatible",
  "name": "ai-proxy",
  "options": {
    "baseURL": "<BASE>",
    "apiKey": "<KEY>"
  },
  "models": {
    "<MODEL_ID>": {
      "name": "DeepSeek V4.1 Flash (ai-proxy)",
      "reasoning": true,
      "limit": { "context": 1000000, "output": 65536 }
    }
  }
}
```

- `name` は人間が `/models` で見るラベル。`(ai-proxy)` を付けて素の提供元と区別する。
- `reasoning` と `limit` は今回のモデルの実測値。別モデルを足すときは値を確認してから書く。
- `cost` は書かない。金額は ai-proxy 側が価格表から計算して `/report` と TUI に出す。

キーを設定ファイルに置きたくない場合だけ、`apiKey` を `"{file:/path/to/key}"` にする
(`{env:...}` はカスタムプロバイダの `apiKey` では解決されない既知の不具合がある)。

### 4. 既定モデルにするか確認する

ユーザーが「これを使え」と言った場合だけ、最上位に 1 行足す。

```jsonc
"model": "ai-proxy/<MODEL_ID>"
```

言われていなければ足さない。

### 5. 検証する

```bash
opencode models | grep ai-proxy
opencode run -m "ai-proxy/<MODEL_ID>" "reply with the single word ok"
```

1 つ目でプロバイダとモデルが見えること、2 つ目でモデルから応答が返ることを確認する。
応答が返ったら、ai-proxy 側にも 1 行記録が残っている(金額は `/report` で見える)。

## うまくいかないとき

| 症状 | 原因 | 対処 |
| --- | --- | --- |
| 401 `invalid_api_key` | キーが違う/失効 | キーを確認する。勝手に作り直さない |
| 404 `not_found` | ベース URL のパス誤り | `/v1` までで止める。`/v1/chat/completions` を書かない |
| 403 `model_not_allowed` | キーの許可リスト外 | `GET /v1/models` の ID だけを使う |
| 404 `model_not_found` | モデル ID の綴り誤り | 同上 |
| 429 `insufficient_quota` | 5 時間 / 週次 / 月次の上限 | `Retry-After` の秒数だけ待つ。上限はユーザーが `key update` で変えられる |
| 502 `upstream_unavailable` | 上流の一時障害 | 少し待って 1 回だけ再試行する |
| `provider not found` | `models` のキーと `-m` の指定がずれている | 両方を `ai-proxy/<MODEL_ID>` で揃える |

## 禁止

- キーをリポジトリやコミットに含めない。共有するならユーザーに確認する。
- 既存のプロバイダ、モデル、プラグイン、`npm` の指定を消さない・書き換えない。
- 設定を丸ごと上書きしない。読んで、足して、書き戻す。
- 検証を通すために、ai-proxy 以外の経路へフォールバックしない。失敗は失敗として報告する。

## 報告すること

1. 変更したファイルの絶対パスと、追記した範囲
2. 使ったベース URL(どちらを選び、なぜか)
3. `/v1/models` が返したモデル ID
4. `opencode run` の結果(応答が出たか)
5. キーは伏せて、末尾 4 文字だけ
