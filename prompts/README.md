# ai-proxy に coding agent を繋ぐ

このディレクトリの各ファイルは、**そのままコーディングエージェントに貼る指示書**である。
やることは 2 つだけ: キーを受け取る、プロンプトを貼る。

| ファイル | 対象 | エージェントが触るファイル |
| --- | --- | --- |
| [`opencode.md`](opencode.md) | opencode | `~/.config/opencode/opencode.jsonc` |
| [`pi.md`](pi.md) | pi | `~/.pi/agent/models.json` |

## 1. キーを受け取る

キーは `sk-aip-` で始まる。発行は管理者が行い、**キー全文は発行時にしか表示されない**
ので、受け取ったらそのまま使う。

キーごとに「使えるモデル」と「トークンの上限」が決まっている。上限は月次・週次・
5 時間の 3 つの窓で、窓ごとに別々にかかる。

## 2. プロンプトを貼る

`opencode.md` か `pi.md` の中身を丸ごとコピーして、キーと一緒にエージェントへ渡す。
設定のマージ、`GET /v1/models` での疎通確認、1 トークンの実走、変更点の報告まで
指示書に入っている。

## 接続先

```
https://router.ht1.top/v1
```

設定するのは `/v1` まで。`/v1/chat/completions` を書くと `/v1/v1/...` になって 404 になる。

## 使えるモデル

キーごとの許可リストで決まる。`GET /v1/models` が返すものがすべてで、それ以外を
投げると 403 `model_not_allowed`。

```bash
curl -sS https://router.ht1.top/v1/models -H "Authorization: Bearer $KEY"
```

2026-11 時点の既定は `deepseek/deepseek-v4.1-flash`(文脈 1,000,000、出力 65,536、
text + image)。

## 課金と消費

USD での金額とトークン消費は管理者側で集計される。クライアントからは見えない。
上限に当たると 429 `insufficient_quota` と `Retry-After`(秒)が返るので、その秒数だけ
待ってから投げ直す。上限を変えたいときは管理者に頼む。

## 検証済みの範囲

2026-11-05 に、公開 URL 経由で両方を実際に通した。指示書に書いてある JSON をそのまま使い、
1 トークンの応答が返り、記録が残ることを確認している。

| クライアント | 確認したこと |
| --- | --- |
| pi | `pi --list-models ai-proxy` に `deepseek/deepseek-v4.1-flash` が出る。`pi --model ai-proxy/deepseek/deepseek-v4.1-flash -p ...` が `ok` を返す |
| opencode | `opencode models` に `ai-proxy/deepseek/deepseek-v4.1-flash` が出る。`opencode run -m ai-proxy/deepseek/deepseek-v4.1-flash "..."` が `ok` を返す |

モデル ID にスラッシュが入っていても、どちらも `プロバイダ/モデル` で正しく解釈する。

## 未検証

- `/v1/responses` 経由で使うクライアント(指示書は chat completions 前提)
- Windows、およびプロキシを経由する社内ネットワーク
- キーのローテーション(古いキーを失効させて新しいキーを配る手順)
