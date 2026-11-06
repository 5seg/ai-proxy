# Connecting a coding agent to ai-proxy

Each file in this directory is a **self-contained instruction sheet meant to be pasted into a
coding agent**. There are only two steps: receive a key, paste the prompt.

| File | Client | File the agent edits |
| --- | --- | --- |
| [`opencode.md`](opencode.md) | opencode | `~/.config/opencode/opencode.jsonc` |
| [`pi.md`](pi.md) | pi | `~/.pi/agent/models.json` |

## 1. Receive a key

Keys start with `sk-aip-`. They are issued by the operator, and **the full key is shown only
once at issue time**, so use it as handed to you.

Each key fixes which models it may call and how many tokens it may spend. Spending is capped
in three separate windows: monthly, weekly, and a rolling 5-hour window.

## 2. Paste the prompt

Copy the whole content of `opencode.md` or `pi.md` and hand it to the agent together with the
key. The sheet covers merging the config, checking connectivity with `GET /v1/models`,
running a one-token request, and reporting what changed.

## Endpoint

```
https://router.ht1.top/v1
```

Configure the base URL up to `/v1`. Writing `/v1/chat/completions` produces `/v1/v1/...` and a
404.

## Available models

Decided per key by its allowlist. Whatever `GET /v1/models` returns is what you may call;
anything else returns 403 `model_not_allowed`.

```bash
curl -sS https://router.ht1.top/v1/models -H "Authorization: Bearer $KEY"
```

As of 2026-11 the default is `deepseek/deepseek-v4.1-flash` (1,000,000 context, 65,536 max
output, text + image input). The list is passed through from the upstream catalog, so fields
such as `context_length` and `max_tokens` appear when the upstream supplies them; do not assume
every entry carries limits.

## Cost and usage

Spend in USD and token counts are aggregated on the operator's side. Clients cannot see them.
When a cap is reached the proxy returns 429 `insufficient_quota` with `Retry-After` in
seconds; wait that long before retrying. Ask the operator to raise a cap.

## Verified

Both clients were driven through the public endpoint on 2026-11-05 using the JSON in these
sheets. A one-token response came back and the usage record was written.

| Client | What was verified |
| --- | --- |
| pi | `pi --list-models ai-proxy` lists `deepseek/deepseek-v4.1-flash`; `pi --model ai-proxy/deepseek/deepseek-v4.1-flash -p ...` replies `ok` |
| opencode | `opencode models` lists `ai-proxy/deepseek/deepseek-v4.1-flash`; `opencode run -m ai-proxy/deepseek/deepseek-v4.1-flash "..."` replies `ok` |

Model IDs contain a slash, and both clients still split `provider/model` correctly.

## Not verified

- Clients that talk to `/v1/responses` (the sheets assume chat completions)
- Windows, and corporate networks that force an HTTP proxy
- Key rotation (revoking an old key and handing out a new one)
