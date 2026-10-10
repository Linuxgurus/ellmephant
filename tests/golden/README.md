# the golden eval set (the lie detector)

Seed set for `ellmephant bench --golden` (PLAN §6 P10). Search quality is one of those things
everyone claims to have and nobody measures, so this directory is where the claims meet the
recall number. Two files:

- `memories.jsonl` — a canonical-JSONL corpus, one memory per line (`key`, `title`, `body`,
  `source`, `tags`). The test harness imports it with the deterministic `HashEmbedder`; a real
  store uses its own rows.
- `queries.jsonl` — one JSON object per line (blank lines and `#` comments ignored):

```json
{"query": "<natural question>", "expect": ["<key>", "..."], "k": 3}
```

`expect` is an accept-set, not an ordered list; recall@k counts a hit when any expected key
lands in the top `k`, and MRR uses the rank of the first. `k` is optional (the run's cutoff
applies when absent).

`ellmephant bench --golden` scores the live store, so every `expect` key must exist there. The
seed corpus is keyword-anchored on purpose — the offline `HashEmbedder` only sees tokens, so
questions share their target's distinctive terms. Grow this toward ≥20 *real* questions from
actual usage and replace the corpus with your own memories; synthetic queries measure the
wrong thing.
