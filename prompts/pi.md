# Task: add the ai-proxy provider to pi

You are a coding agent. Add exactly one provider named `ai-proxy` to the user's pi and leave it
usable. **Do not change existing provider configuration.** Your job is to merge, not to replace.

## Inputs

- **API key**: `sk-aip-...`. It is either inline in this prompt or in the environment
  variable `AIPROXY_KEY`. If neither exists, do not guess — say the key is missing and stop.
- **Base URL**: `https://router.ht1.top/v1`, fixed.
- **Models**: never hardcode IDs. Use only what `GET /v1/models` returns in step 2.

## Steps

### 1. Inspect the existing config

The target is `~/.pi/agent/models.json` and it may not exist yet. Look before you write:

```bash
python3 -c 'import json,os;p=os.path.expanduser("~/.pi/agent/models.json");print(sorted(json.load(open(p))["providers"])) if os.path.exists(p) else print("missing")'
```

Two rules matter more than the tool you pick:

1. Merge into the existing object. Never rebuild the file from scratch, never drop another
   provider.
2. Write to a temporary file in the same directory, then rename it over the original. Shell
   redirection of the form `tool file > file` truncates the file before reading it.

Reformatting existing entries (indentation, key order) is acceptable. Losing a key is not.

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

The provider object has this shape. Replace `<MODEL_ID>` with an ID from step 2 and
`<MODEL_NAME>` with a short human label derived from it (`deepseek/deepseek-v4.1-flash` becomes
`DeepSeek V4.1 Flash`). Repeat the `models` entry once per ID.

```json
{
  "name": "ai-proxy",
  "baseUrl": "https://router.ht1.top/v1",
  "api": "openai-completions",
  "apiKey": "$AIPROXY_KEY",
  "compat": { "supportsDeveloperRole": false },
  "models": [
    {
      "id": "<MODEL_ID>",
      "name": "<MODEL_NAME> (ai-proxy)",
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
      "cost": { "input": 0.15, "output": 0.6, "cacheRead": 0.003, "cacheWrite": 0 }
    }
  ]
}
```

The following is a complete, idempotent merge that works on a machine with `python3` and no
`jq`. Edit the literal values in place, then run it. It creates the file when missing, keeps
every other provider, and never truncates the file it reads:

```bash
python3 - <<'PY'
import json, os, tempfile

path = os.path.expanduser("~/.pi/agent/models.json")
if os.path.exists(path):
    with open(path) as f:
        cfg = json.load(f)
else:
    cfg = {"$schema": "https://pi.dev/schemas/models.schema.json", "providers": {}}

cfg.setdefault("providers", {})["ai-proxy"] = {
    "name": "ai-proxy",
    "baseUrl": "https://router.ht1.top/v1",
    "api": "openai-completions",
    "apiKey": "$AIPROXY_KEY",
    "compat": {"supportsDeveloperRole": False},
    "models": [
        {
            "id": "<MODEL_ID>",
            "name": "<MODEL_NAME> (ai-proxy)",
            "reasoning": True,
            "thinkingLevelMap": {"off": "off", "minimal": None, "low": "low",
                                 "medium": None, "high": "high", "xhigh": None, "max": "max"},
            "contextWindow": 1000000,
            "maxTokens": 65536,
            "input": ["text", "image"],
            "cost": {"input": 0.15, "output": 0.6, "cacheRead": 0.003, "cacheWrite": 0},
        }
    ],
}

os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(os.path.abspath(path)))
with os.fdopen(fd, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
os.replace(tmp, path)
PY
chmod 600 ~/.pi/agent/models.json
```

Field rules:

- The `contextWindow`, `maxTokens`, and `cost` numbers above are the upstream specification of
  `deepseek/deepseek-v4.1-flash`, the default model. For any other ID do not copy them blindly:
  drop `thinkingLevelMap` and `cost`, keep `contextWindow` and `maxTokens` only if the catalog in
  step 2 states them, and otherwise ask the operator.
- `compat.supportsDeveloperRole` is off because the upstream rejects the `developer` role. The
  proxy forwards request bodies verbatim, so this is the client's job to get right.
- If the key arrives through the environment, keep `apiKey` as `"$AIPROXY_KEY"`; pi expands
  `$NAME` and `${NAME}`. If it was handed to you inline, write the literal string instead.
- `id` gets combined as `provider/id`, so it contains a slash. pi treats `ai-proxy` as the
  provider segment; leave it as is.
- `name` is the label in pi's `/model` selector. Keep the `(ai-proxy)` suffix so it is not
  confused with the direct provider.
- The base URL stops at `/v1`. Writing `/v1/chat/completions` produces `/v1/v1/...` and a 404.

### 4. Make it the default only if asked

If and only if the user said to use this model, set the default model in
`~/.pi/agent/settings.json` to `ai-proxy/<MODEL_ID>`. Otherwise do not touch that file.

### 5. Verify

```bash
pi --list-models ai-proxy
pi --model "ai-proxy/<MODEL_ID>" -p "reply with the single word ok"
```

The first must list the model; the second must produce a response. If the user picks models in
the TUI, reopening `/model` reloads `models.json`.

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
| Model missing from `/model` | authentication did not resolve | Check that `apiKey` expands to a non-empty value; `pi --list-models` breaks too |

## Forbidden

- Do not put the key into a repository or a commit. Ask the user before sharing it.
- Do not delete or rewrite existing providers, models, `compat`, or `modelOverrides`.
- Do not overwrite `models.json` wholesale, and do not read and write it through the same open
  file.
- Do not fall back to another endpoint to make verification pass. Report failures as failures.

## Report

1. Absolute path of every file changed, and the range you added
2. The model IDs `GET /v1/models` returned
3. The result of `pi --model ... -p` (did a response come back?)
4. The key masked to its last four characters
