# golden eval set

Seed set for `llmphant bench --golden` (PLAN §6 P10). One JSON object per line:

```json
{"query": "<natural question>", "expect": ["<key>", "..."], "k": 3}
```

`expect` is an accept-set, not an ordered list; recall@k counts a hit when any
expected key lands in the top `k`. Grow this toward ≥20 real questions taken from
actual usage — synthetic queries measure the wrong thing.
