# タスク: pi に ai-proxy プロバイダを追加する

あなたはコーディングエージェントである。ユーザーの pi に `ai-proxy` という
プロバイダを 1 つ追加し、そこ経由でモデルが使える状態にする。**既存のプロバイダ設定は
1 文字も変えない。** 追記とマージだけを行う。

## 入力

- **API キー**: `sk-aip-...`。このプロンプトに書かれているか、環境変数 `AIPROXY_KEY`
  にある。どちらも無ければ推測せず、「キーが見つからない」と言って止まる。
- **ベース URL**: `https://router.ht1.top/v1` 固定。
- **モデル**: 決め打ちしない。下の手順 2 で `GET /v1/models` が返したものだけを書く。

## 手順

### 1. 既存設定を読む

対象は `~/.pi/agent/models.json`。無ければ
`{ "$schema": "https://pi.dev/schemas/models.schema.json", "providers": {} }` から作る。

JSON(コメント無し)なので `jq` か何かで読み、`providers` に 1 キー足して書き戻す。
既存の `providers` 配下には触らない。

```bash
jq '.providers | keys' ~/.pi/agent/models.json
```

### 2. このキーで使えるモデルを確認する

```bash
curl -sS -o /tmp/aiproxy-models.json -w '%{http_code}\n' \
  https://router.ht1.top/v1/models -H "Authorization: Bearer $KEY"
```

- `200`: `/tmp/aiproxy-models.json` の `data[].id` がこのキーで使えるモデル。
- `401`: キーが違うか失効している。止まる。
- 接続できない: ネットワークを疑う。URL は上記で固定なので、変えずに報告する。

### 3. プロバイダを追記する

`providers` に `ai-proxy` を足す。`<MODEL_ID>` は手順 2 で得た ID(複数あれば全部)に
置き換える。

```json
"ai-proxy": {
  "name": "ai-proxy",
  "baseUrl": "https://router.ht1.top/v1",
  "api": "openai-completions",
  "apiKey": "$AIPROXY_KEY",
  "compat": {
    "supportsDeveloperRole": false
  },
  "models": [
    {
      "id": "deepseek/deepseek-v4.1-flash",
      "name": "DeepSeek V4.1 Flash (ai-proxy)",
      "reasoning": true,
      "thinkingLevelMap": {
        "off": "off",
        "minimal": null,
        "low": "low",
        "medium": null,
        "high": "high",
        "xhigh": null,
        "max": "max"
      },
      "contextWindow": 1000000,
      "maxTokens": 65536,
      "input": ["text", "image"],
      "cost": {
        "input": 0.15,
        "output": 0.6,
        "cacheRead": 0.003,
        "cacheWrite": 0
      }
    }
  ]
}
```

- `apiKey` は、キーを環境変数で渡されたなら `"$AIPROXY_KEY"` のまま
  (pi は `$NAME` / `${NAME}` を環境変数で展開する)。プロンプトに直書きで渡されたなら
  その文字列にする。どちらの場合もファイルのパーミッションを `600` にする。
- `id` は `provider/id` の形なのでスラッシュを含む。pi 側は `ai-proxy` を
  プロバイダ名として解釈するので、このままでよい。
- `name` は人間が `/model` で見るラベル。`(ai-proxy)` を付けて素の提供元と区別する。
- `contextWindow` / `maxTokens` / `input` / `cost` は今回のモデルの実測値。
  別モデルを足すときは値を確認してから書く。`cost` の単位は USD / 100 万トークン。

`baseUrl` は `/v1` まで。`/v1/chat/completions` を書くと `/v1/v1/...` になって 404 になる。

### 4. 既定モデルにするか確認する

ユーザーが「これを使え」と言った場合だけ、`~/.pi/agent/settings.json` の既定モデルを
`ai-proxy/<MODEL_ID>` に変える。言われていなければ触らない。

### 5. 検証する

```bash
pi --list-models ai-proxy
pi --model "ai-proxy/<MODEL_ID>" -p "reply with the single word ok"
```

1 つ目にモデルが出ること、2 つ目で応答が返ることを確認する。`/model` を開き直すと
`models.json` が再読込されるので、TUI から選ぶ場合も一度開き直す。

## うまくいかないとき

| 症状 | 原因 | 対処 |
| --- | --- | --- |
| 401 `invalid_api_key` | キーが違う/失効 | キーを確認する。勝手に作り直さない |
| 404 `not_found` | ベース URL のパス誤り | `/v1` までで止める |
| 403 `model_not_allowed` | キーの許可リスト外 | `GET /v1/models` の ID だけを使う |
| 404 `model_not_found` | モデル ID の綴り誤り | 同上 |
| 429 `insufficient_quota` | 月次 / 週次 / 5 時間の上限 | `Retry-After` の秒数だけ待つ。上げたいなら管理者に頼む |
| 502 `upstream_unavailable` | 上流の一時障害 | 少し待って 1 回だけ再試行する |
| モデルが `/model` に出ない | 認証が解決できていない | `apiKey` の展開結果が空でないか確認する(`pi --list-models` でも落ちる) |

## 禁止

- キーをリポジトリやコミットに含めない。共有するならユーザーに確認する。
- 既存のプロバイダ、モデル、`compat`、`modelOverrides` を消さない・書き換えない。
- `models.json` を丸ごと上書きしない。読んで、足して、書き戻す。
- 検証を通すために、別のエンドポイントへフォールバックしない。失敗は失敗として報告する。

## 報告すること

1. 変更したファイルの絶対パスと、追記した範囲
2. `/v1/models` が返したモデル ID
3. `pi --model ... -p` の結果(応答が出たか)
4. キーは伏せて、末尾 4 文字だけ
