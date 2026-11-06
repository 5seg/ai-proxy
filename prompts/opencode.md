# Task: add the ai-proxy provider to opencode

You are a coding agent. Add exactly one provider named `ai-proxy` to the user's opencode and
leave it usable. **Do not change existing provider configuration.** Your job is to merge, not
to replace.

## Inputs

- **API key**: `sk-aip-...`. It is either inline in this prompt or in the environment
  variable `AIPROXY_KEY`. If neither exists, do not guess — say the key is missing and stop.
- **Base URL**: `https://router.ht1.top/v1`, fixed.
- **Models**: never hardcode IDs. Use only what `GET /v1/models` returns in step 2.

## Steps

### 1. Read the existing config

Use the first file that exists, in this order:

1. `~/.config/opencode/opencode.jsonc`
2. `~/.config/opencode/opencode.json`
3. If neither exists, create `~/.config/opencode/opencode.json`

`.jsonc` allows comments. Use a JSONC-aware reader so the comments survive, and never write
stripped JSON back into a `.jsonc` file. If you have no JSONC-aware tool, edit only the
`provider` object as text and leave every comment and every other key alone (`$schema`,
`instructions`, `plugin`, `mcp`, `permission`, and so on).

In both cases the existing `provider` entries are merge targets, never replacement targets.
Write to a temporary file and rename it over the original, for example
`mv ~/.config/opencode/opencode.json.tmp ~/.config/opencode/opencode.json`. Redirecting into the
file you are reading (`tool file > file`) truncates it before the tool sees it.

### 2. Discover the models this key may call

```bash
curl -sS -o /tmp/aiproxy-models.json -w '%{http_code}\n' \
  https://router.ht1.top/v1/models -H "Authorization: Bearer $KEY"
```

- `200`: every `data[].id` in `/tmp/aiproxy-models.json` is callable with this key.
- `401`: the key is wrong or revoked. Stop.
- Connection failure: suspect the network. The URL is fixed, so report the failure instead of
  substituting a different endpoint.

Entries are passed through from the upstream catalog, so fields such as `context_length` and
`max_tokens` may be present. Prefer those values over the ones in step 3 when they are there.

### 3. Add the provider

Add `ai-proxy` inside the `provider` object. Replace `<MODEL_ID>` with an ID from step 2 and
`<MODEL_NAME>` with a short human label derived from it (for example
`deepseek/deepseek-v4.1-flash` becomes `DeepSeek V4.1 Flash`). Repeat the `models` entry once
per ID.

```jsonc
"ai-proxy": {
  "npm": "@ai-sdk/openai-compatible",
  "name": "ai-proxy",
  "options": {
    "baseURL": "https://router.ht1.top/v1",
    "apiKey": "<KEY>"
  },
  "models": {
    "<MODEL_ID>": {
      "name": "<MODEL_NAME> (ai-proxy)",
      "reasoning": true,
      "limit": { "context": 1000000, "output": 65536 }
    }
  }
}
```

- `reasoning` and `limit` above are the upstream specification of `deepseek/deepseek-v4.1-flash`,
  the default model. For any other ID, set `limit` only from values the catalog states in step 2,
  and ask the operator when it is silent.
- `name` is the label in `opencode models`. Keep the `(ai-proxy)` suffix so it is not confused
  with the direct provider.
- Do not set `cost`. Spend is calculated on the server side.
- If the user will not keep the key in the config file, use `"{file:/path/to/key}"` for `apiKey`,
  put the key in `~/.config/opencode/aiproxy.key`, and `chmod 600` that file. (`{env:...}` does
  not resolve inside a custom provider's `apiKey`; that is a known bug.)
- The base URL stops at `/v1`. Writing `/v1/chat/completions` produces `/v1/v1/...` and a 404.

### 4. Make it the default only if asked

If and only if the user said to use this model, add one top-level line:

```jsonc
"model": "ai-proxy/<MODEL_ID>"
```

Otherwise leave the default model untouched.

### 5. Verify

```bash
opencode models | grep ai-proxy
opencode run -m "ai-proxy/<MODEL_ID>" "reply with the single word ok"
```

The first must list the provider and model; the second must produce a model response.

## When it fails

| Symptom | Cause | Action |
| --- | --- | --- |
| 400 `invalid_request` | malformed JSON body, or no `model` field | Fix the request shape |
| 401 `invalid_api_key` | wrong or revoked key | Check the key. Never mint your own |
| 403 `model_not_allowed` | model not on this key's allowlist | Use only IDs from `GET /v1/models` |
| 404 `not_found` | base URL carries a path past `/v1`, or the endpoint is not proxied | Stop the base URL at `/v1` |
| 404 `model_not_found` | typo in the model ID, or the ID is not on this key | Compare against step 2 |
| 429 `insufficient_quota` | monthly, weekly, or rolling 5-hour cap hit | Wait out `Retry-After` in seconds; ask the operator to raise the cap |
| 500 `internal_error` | proxy-side database or lock failure | Report to the operator; do not retry in a loop |
| 502 `upstream_unavailable` | transient upstream failure, no `Retry-After` sent | Wait a few seconds, retry once |
| `provider not found` | the `models` key and the `-m` argument disagree | Use `ai-proxy/<MODEL_ID>` in both places |

## Forbidden

- Do not put the key into a repository or a commit. Ask the user before sharing it.
- Do not delete or rewrite existing providers, models, plugins, or `npm` entries.
- Do not overwrite the config wholesale, and do not drop comments from a `.jsonc` file.
- Do not fall back to another endpoint to make verification pass. Report failures as failures.

## Report

1. Absolute path of every file changed, and the range you added
2. The model IDs `GET /v1/models` returned
3. The result of `opencode run` (did a response come back?)
4. The key masked to its last four characters
