# ai-proxy に coding agent を繋ぐ

このディレクトリの各ファイルは、**そのままコーディングエージェントに貼る指示書**である。
人間がやることは 2 つだけ: キーを発行する、プロンプトを貼る。

| ファイル | 対象 | エージェントが触るファイル |
| --- | --- | --- |
| [`opencode.md`](opencode.md) | opencode | `~/.config/opencode/opencode.jsonc` |
| [`pi.md`](pi.md) | pi | `~/.pi/agent/models.json` |

## 1. キーを発行する

上限は 3 つの窓で別々にかけられる(どれも省略可、省略した窓は上限なし)。端末で
フラグを付けずに実行すると対話で聞いてくる。

```bash
ssh vl
sudo /usr/local/bin/ai-proxy --config /etc/ai-proxy/config.toml key add --device laptop
# 例: --limit 100M --weekly 25M --limit-5h 2.5M
```

キー全文(`sk-aip-...`)はこのときしか出ない。控えてプロンプトに貼る。

## 2. プロンプトを貼る

`opencode.md` か `pi.md` の中身を丸ごとコピーして、キーと一緒にエージェントへ渡す。
設定のマージ、`GET /v1/models` での疎通確認、1 トークンの実走、変更点の報告まで
指示書に入っている。

## 接続先

| 経路 | ベース URL | 使える場所 |
| --- | --- | --- |
| Tailscale | `http://100.127.113.52:8787/v1` | テールネット内のみ |
| 公開(Cloudflare) | `https://router.ht1.top/v1` | どこからでも |

設定するのは `/v1` まで。`/v1/chat/completions` を書くと `/v1/v1/...` になって 404 になる。

## 使えるモデル

キーごとの許可リストで決まる。`GET /v1/models` が返すものがすべてで、それ以外を
投げると 403 `model_not_allowed`。

```bash
curl -sS http://100.127.113.52:8787/v1/models -H "Authorization: Bearer $KEY"
```

2026-11 時点の既定は `deepseek/deepseek-v4.1-flash`(文脈 1,000,000、出力 65,536、
text + image)。`flash-fast` は許可していない。

## 課金と消費

金額は USD で `/report` と TUI に出る。トークンの消費は端末(キー)単位で記録され、
月次・週次・5 時間の上限で止まる。上限に当たると 429 `insufficient_quota` と
`Retry-After`(秒)が返る。

## 検証済みの範囲

2026-11-05 に公開 URL 経由で両方を実際に通した。指示書に書いてある JSON をそのまま使い、
1 トークンの応答が返り、`request` に 1 行ずつ記録されることを確認している。

| クライアント | 確認したこと |
| --- | --- |
| pi | `pi --list-models ai-proxy` に `deepseek/deepseek-v4.1-flash` が出る。`pi --model ai-proxy/deepseek/deepseek-v4.1-flash -p ...` が `ok` を返す |
| opencode | `opencode models` に `ai-proxy/deepseek/deepseek-v4.1-flash` が出る。`opencode run -m ai-proxy/deepseek/deepseek-v4.1-flash "..."` が `ok` を返す |

モデル ID にスラッシュが入っていても、どちらも `プロバイダ/モデル` で正しく解釈する。

## 未検証

- `/v1/responses` 経由で使うクライアント(指示書は chat completions 前提)
- Windows、およびプロキシを経由する社内ネットワーク
- キーのローテーション(古いキーを `key revoke` して新しいキーを配る手順)
