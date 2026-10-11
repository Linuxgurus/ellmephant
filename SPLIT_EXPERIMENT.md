# Handoff — "detect oversized memories worth splitting" experiment

**Status:** ✅ COMPLETE — binary run (43/43) in §7.2. The follow-on **scoring** work is **Part 2 (§12–21, end of file)**: winner = **SplitScore**; status = **proposal, nothing built**.
**Progress:** done. Picking up cold? Read §0 TL;DR, §7.2, then **Part 2**.
**Purpose:** everything needed to pick this experiment up cold, in a fresh context, and finish it.

---

## 0. TL;DR

We are building a *finder* that flags memories which have grown into **grab-bags** (multiple distinct topics in one body) so they can be split. Established so far:

- **Do NOT detect grab-bags with embedding geometry.** Every geometric idea (isolation, average distance, link count, telemetry) is confounded or points the wrong way. See §4.
- **Winning shape: cheap recall filter (size) → an LLM judge reads the prose → human commits by hand.** Mirrors the existing `memory_dupes` "finder, not judge" contract.
- **The rubric (prompt) IS the verdict.** Same memories, different framing → polarity flips 5/5 SPLIT to 0/5 SPLIT. The rubric is the real artifact and must carry an explicit constitution (skepticism + atom-viability floor).
- **Two independent dials:** (1) *bias* (keep ↔ split) and (2) *atom hygiene* (loose ↔ single-intent + standalone). The open decision is where to set the bias for the fat tail.
- **Batching several memories into one judge SUPPRESSES SPLIT verdicts.** One agent per (memory × rubric) is the reliable method.
- **Candidate rubrics still in play: R2 (willing) and R5 (conservative).** Everything else is a control or a near-twin.

---

## 1. Background / the problem

`ellmephant` is a semantic memory store. Its doctrine (SPEC.md §9, ENGINEERING.md) is **one memory = one topic**:

> "One memory = one topic: terse and self-contained, close facts one per line; distinct facts split. A coherent topic embeds sharply and edits surgically; a grab-bag summary smears the vector and rots."

Some memories grow into **grab-bags** — a namespace/overview note that accumulates distinct sub-topics. The row's single embedding gets *smeared*, so it matches many queries weakly and pollutes search. Goal of the experiment: **find** those rows reliably and cheaply so a human/agent can split them.

**No code written yet.** Design + validation only. A winning design would be built later as e.g. `store::oversized` / a `memory_big` MCP tool + a read-time hint on `memory_get` (see §10).

---

## 2. The store + doctrine

- One document per memory is embedded: `key` + `tags` + `title` + `body`, with `[[links]]` stripped (`embed::projection::embed_document`).
- Search is semantic-only KNN (cosine distance; default ceiling `DEFAULT_MAX_DISTANCE = 0.6`). No FTS/hybrid.
- **Merge** already has a finder: MCP `memory_dupes` (`src/store/dupes.rs`) — "finder, not judge", pairs whose *whole-document* vectors sit within a tight ceiling (`DEFAULT_MAX_DISTANCE = 0.05`).
- **Split** is the missing counterpart — this experiment.
- Doctrine: merge/split happen **by hand** (fold weaker into stronger + `memory_delete`; or carve out new memories). Any split tool must be a *proposer*, never an auto-writer.
- The three §9 rot modes (if we ever cover all):
  1. `memory_dupes` → **merge** (whole memories too close).
  2. split finder → **split** (a body covering ≥2 topics).
  3. fact-router → **move** (a fact closer to *another* memory than its own) — explored, deferred (§4.1).

**EMBEDDING ASYMMETRY LANDMINE:** Qwen3 is asymmetric — `embed_query()` prepends `"Instruct: …\nQuery: "`; `embed_one()`/`embed()` (documents) do NOT (`src/embed/qwen3.rs`). **Any two vectors you compare must be on the same side.** Comparing a query-embedded title to document-embedded body lines = garbage. All document-side.

---

## 3. What we are deciding

Given a memory, classify **SPLIT** or **COHERE** — and if SPLIT, propose atom keys. Deliverable: a **shortlist of genuine grab-bags** + a **default rubric** to apply.

There is **NO ground truth** — the only truth is James reading the memory. So the experiment is (a) find where to set the split threshold and (b) produce the consensus split list.

---

## 4. Ideas tested and their verdicts

### 4.1 DEAD ideas (do not build these)

| idea | why it died |
|---|---|
| **"big + far from everything" = grab-bag** | Isolation measures *novelty*, which is good. Also a blend of common topics sits in the *middle* of the store, not far out. Points the wrong way. |
| **"few links" strengthens the signal** | Confounded both directions: grab-bags name-drop many `[[keys]]` (high out-degree); hubs/overviews (blessed by §9) have high in-degree. Sick either way. |
| **telemetry (high `visits` / low `times_useful`) flags bloat** | Backwards. A grab-bag's blended vector wedges into *many* searches → high `visits`; the agent finds *something* → high `times_useful`. The garbage can is employee of the month. Telemetry *masks* the problem. |
| **"average distance from everything"** | A centrality measure, not a grab-bag measure; both signs confounded. |
| **"lines = facts" (per-line chunk embedding + clustering)** | Most fat memories are **single-line prose blobs** (`network-gateway-dns`, `documents-ocis`, `identity-authentik`, `ellmephant-mcp` = 1 line). Nothing to chunk by line. Short-line embeddings are noisy anyway. Only an LLM reading prose handles this. |
| **fact-router (line ↔ other memories' lines; move misplaced facts)** | Sees the *move* failure mode, but needs a store-wide line-embedding pass, a calibrated margin, and can't tell "move" from "cross-link". Over-engineered at ~107 rows. Revisit only if misplaced facts become a recurring pain. |

### 4.2 LIVE ideas (survived)

- **Size is a cheap, high-recall pre-filter.** No grab-bag is small. Length is a *gate*, not a detector.
- **An LLM judge reads the prose** — the only thing that reliably identifies "two topics or one" and produces atoms + a retitle-vs-split call.
- **Neighbour-dispersion ("bridge score")** — a memory whose top-k nearest neighbours are mutually far apart spans communities. Vectors-only (reuses the `dupes` KNN pass). Kept as an optional *re-ranker*, not a detector. Unnecessary at ~107 rows.
- **Read-time hint** — `memory_get` already returns the body; a one-line "2.4 kb / 30 lines — split?" lands the judgement where the content is. Zero new infra. Not built.
- **Authoring-time nudge** — ✅ **CHOSEN v0 (built 2026-10-11).** Promote the passive "split distinct" rule in `UPSERT_DESCRIPTION` to an *active pre-write check*: the writer (an LLM, body in hand) evaluates ≥2 distinct retrieval intents and, if so, upserts atoms as **separate calls** (never a misc bucket; each ≥2-3 facts). Rationale: zero new infra; the winning mechanism (LLM reads prose) moved earlier; and tool *descriptions* survive on Cline/TypingMind where server `instructions` are dropped (`[[cline-drops-mcp-instructions]]`, `[[typingmind-connector-drops-mcp-instructions]]`). **Additive to** the finder, not a replacement — covers only post-deploy agent writes (not the existing fat rows, not programmatic upserts). Risk = advisory text the model can wave through (the R1 rubber-stamp failure mode) → must be *measured*. See §22.
- **Recommended architecture:** dumb size finder (recall) → LLM judge with a written constitution → human commits by hand. Plus optional read-time hint.

---

## 5. The rulesets (rubrics)

Each rubric is a **system prompt** given to a dedicated judge agent, with one memory as the task. Copy-paste exactly to reproduce.

### B — baseline / eager (CONTROL, not a candidate)
> You are a memory-store curation judge. Doctrine: one memory = ONE topic; a memory that mixes DISTINCT topics smears its single embedding and should be SPLIT. Judge topic COHERENCE, not length. Reply in EXACTLY two lines max: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated proposed atom keys. Nothing else.

### R1 — anti-fragmentation, skeptical default-keep (DEAD)
> You are a SKEPTICAL memory-store curation judge. Default verdict is COHERE. Split ONLY for ≥2 genuinely DISTINCT retrieval intents (never mere facets). Do not maximise atom count; splitting is costly (fragments the link graph, spawns orphans). Reply EXACTLY: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated atom keys. Nothing else.

*(R1 scored 0/5 on the pilot — a frozen rubber stamp. Dead.)*

### R2 — viability floor (CANDIDATE: the "willing" setting)
> You are a memory-store curation judge. You MAY split a memory, but ONLY into pieces that STAND ALONE as their own retrieval target: roughly ≥2-3 facts and ≥~200 chars each. NEVER mint a 1-fact or ~1-line memory; any fragment too small folds back into the parent. Reply EXACTLY: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated atom keys. Nothing else.

### R3 — skeptical default-keep + floor (near-twin of R5)
> You are a SKEPTICAL memory-store curation judge. Default verdict is COHERE. Split ONLY when ≥2 genuinely DISTINCT retrieval intents exist (never mere facets). Every proposed atom must ALSO stand alone (≥2-3 facts, ≥~200 chars). Reply EXACTLY: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated atom keys. Nothing else.

### R4 — split-first + hygiene (DEAD: eager but clean)
> You are a memory-store curation judge. Method is SPLIT-FIRST — do NOT default to keep. If ≥2 genuinely distinct retrieval intents exist, you WILL split. But every resulting atom must itself be SINGLE-INTENT (no 'gotchas'/misc buckets — re-cut so each fact lands in its true home) AND STAND ALONE (≥2-3 facts, ≥~200 chars). Reply EXACTLY: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated atom keys. Nothing else.

*(R4 fixed atom hygiene but kept the eager bias → split everything, cleanly. Dead as a decision rule.)*

### R5 — necessity + hygiene (CANDIDATE: the "conservative" setting)
> You are a memory-store curation judge. Split ONLY when keeping the facts together ACTIVELY HARMS retrieval — i.e. ≥2 genuinely distinct intents that mislead each other. Every resulting atom must be SINGLE-INTENT (no 'gotchas'/misc buckets — re-cut so each fact has a true home) AND STAND ALONE (≥2-3 facts, ≥~200 chars); facts with no home here are pushed to a linked memory, not bucketed. Reply EXACTLY: line1 = SPLIT or COHERE; line2 (only if SPLIT) = comma-separated atom keys. Nothing else.

### The two dials (mental model)
- **Bias dial** (keep ↔ split): R1 (dead: frozen keep) · R3/R5 (strict) · R2 (medium) · R4/B (eager).
- **Hygiene dial** (loose ↔ single-intent+standalone): B/R1 (loose) · R2/R3 (moderate) · R4/R5 (strict).
- **R2 = medium bias + decent hygiene. R5 = strict bias + strict hygiene.** These are the two candidates. R3 ≈ R5 in strictness but weaker construction.

---

## 6. Experimental method

- **Judge = a sub-agent** (the `spawn_agent` / `team_*` machinery). No external LLM API. The judge gets one rubric (system prompt) and one memory (task). It replies `SPLIT|COHERE` (+ atom keys).
- **One agent per (memory × rubric).** This is the reliable unit.
- **BATCHING IS A CONFOUND — do not batch.** Putting several memories in one judge context *suppresses* SPLIT verdicts (a 27-memory batch split only 2/27, while solo agents split ~85% of the same fat set). Grouped judging also lets the judge anchor across items. Always one memory per agent.
- **No ground truth.** "Correct" is James's read of the memory. Everything below is measured against *my own* LLM read, which is itself another opinion — treat results as **directions, not statistics**.
- Reference "truth" used for scoring the pilot: **SPLIT** = `network-gateway-dns`, `documents-ocis`; **KEEP** = `seaweedfs-ec-vacuum` (borderline), `identity-authentik`, `ellmephant-mcp`.

---

## 7. Results so far

### 7.1 Early pilots (bounded, for context)

| run | setup | outcome |
|---|---|---|
| **Pilot A** | 5 biggest memories, one agent each, NO rubric ("split distinct topics") | **5/5 SPLIT** (~28 atoms) — everything splits |
| **Pilot B** | 5 biggest, one agent per *rubric* (batched over 5) | B 5/5 · R1 0/5 · R2 3/5 · R3 1/5 |
| **Pilot R4** | 5 biggest, one agent | **5/5 SPLIT** (10 atoms) — clean but eager |
| **Pilot R5** | 5 biggest, one agent | **1/5 SPLIT** (`network-gateway-dns` only) |
| **Batched baseline** | 27 memories in ONE agent | **2/27 SPLIT** ← the batching suppression evidence |

### 7.2 Full run — 43/43 memories — COMPLETE

The 13 ≥1500-char memories were run against all four rubrics (B·R2·R3·R5); the other 30 against {R2, R5} only. Machine ledger: `.tmp/split-exp/RESULTS.tsv`.

**Only 8 memories were flagged by a candidate rubric** (the other 35 are COHERE under both):

| memory | R2 | R5 | flagged by |
|---|---|---|---|
| finance-robinhood-data-gotchas | SPLIT | SPLIT | **both** |
| ellmephant-gotchas | SPLIT | SPLIT | **both** |
| seaweedfs-ec-vacuum | SPLIT | COHERE | R2 |
| documents-ocis | SPLIT | COHERE | R2 |
| github-chart-release-pattern | SPLIT | COHERE | R2 |
| seaweedfs-fork | SPLIT | COHERE | R2 |
| network-gateway-dns | COHERE | SPLIT | R5 |
| cluster-diagnostics | COHERE | SPLIT | R5 |

**Tallies (of 43): B 11 · R2 6 · R3 1 · R5 4 · union(R2,R5) 8 · agreement 2.**

**Read:** the two candidates disagree on 6 of the 8. R2 is "willing" (6), R5 is "conservative" (4); they AGREE only on the two obvious grab-bags. `network-gateway-dns` — judged a should-split by eye — was caught by **R5 only**, while R2 caught several R5 passed. Neither dominates. Use the **union (8)** as the human-review shortlist; the **agreement pair** (`finance-robinhood-data-gotchas`, `ellmephant-gotchas`) as highest confidence.

**Shortlist (union, 8):** `seaweedfs-ec-vacuum`, `network-gateway-dns`, `documents-ocis`, `finance-robinhood-data-gotchas`, `github-chart-release-pattern`, `seaweedfs-fork`, `ellmephant-gotchas`, `cluster-diagnostics`.

Full atom keys: Appendix A.

---

## 8. Threshold & roster

### 8.1 Size histogram (store = 107 memories, 102k body chars, avg ~953)

| bucket | n |
|---|---|
| ≥1500 | 13 |
| 1000–1499 | 30 |
| 700–999 | 33 |
| 400–699 | 21 |
| <400 | 10 |

Cumulative: ≥700 → 76 · ≥900 → 53 · **≥1000 → 43** · ≥1200 → 30 · ≥1500 → 13.

### 8.2 DECISION: target set = ≥1000 chars (43 memories)

Rationale: a grab-bag needs room for ≥2 viable atoms at ~200+ chars each, so sub-1000 bodies essentially cannot qualify. Cost of the full 4-rubric run collapsed from 428 → 86 agents (43 × {R2,R5}); **all 43 are now DONE** (§7.2).

### 8.3 The roster (43 memories — ALL tested; ✅ = done)

```
✅ seaweedfs-ec-vacuum               2209
✅ network-gateway-dns               2154
✅ documents-ocis                    2121
✅ identity-authentik                1875
✅ ellmephant-mcp                    1765
✅ seaweedfs-build-deploy            1756
✅ seaweedfs-open-work               1731
✅ finance-robinhood-data-gotchas    1662
✅ finance-other-brokerage-stock-docs 1628
✅ ellmephant-helm-chart             1565
✅ ellmephant-write-policy           1550
✅ cline-mcp-reconnect               1525
✅ finance-prior-years-reconstruction 1501
   storage-seaweedfs                 1495
   mcp-typingmind-connector          1468
   github-chart-release-pattern      1438
   finance-household-income          1409
   lance-namespace-api               1404
   ellmephant-search                 1383
   news-kb-entity-resolution         1371
   seaweedfs-fork                    1371
   finance-robinhood-pnl-scripts     1353
   observability-stack               1348
   todo-washsale-backcheck-2024-2025 1292
   ellmephant-connection-timeouts    1285
   ellmephant-gotchas                1283
   ai-ollama-stack                   1282
   backups-velero                    1263
   james-communication-style         1243
   ngf-client-keepalive              1223
   taxes-2026-investment-db          1196
   macos-local-network-privacy       1178
   james-health-conditions           1109
   james-family                      1097
   todo-wp-chart-refresh             1086
   ngoc-us-immigration               1067
   cluster-diagnostics               1054
   finance-tax-profile               1050
   ellmephant-maintenance            1036
   finance-trading-style             1033
   todo-2026-year-end-futures-mtm    1026
   media-stack                       1016
   ellmephant-container              1004
```

---

## 9. Corpus & tooling (how the data was obtained)

- **Live store is in-cluster**, NOT `~/.ellmephant/memory.db` (that host file is a separate, stale/empty store). Prod = the `ellmephant` deployment on a RWO PVC mounted `/data`, ns `mcp`.
- **Copy the store out read-only** (WAL allows concurrent readers):
  ```sh
  kubectl get pods -n mcp | grep ellmephant          # get the current pod name
  mkdir -p .tmp/live
  kubectl -n mcp cp <pod>:/data/memory.db      .tmp/live/memory.db
  kubectl -n mcp cp <pod>:/data/memory.db-wal  .tmp/live/memory.db-wal
  kubectl -n mcp cp <pod>:/data/memory.db-shm  .tmp/live/memory.db-shm
  ```
- **Throwaway copy lives at** `.tmp/live/memory.db` (a snapshot — safe to delete; NEVER write to the live store).
- **Corpus dump** (107 memories, size-descending) at `.tmp/corpus/all.txt`, format per record:
  `@@@ <key> | <chars>c | <lines>L | <title>` then the body.
- **Query it** with the local `sqlite3` binary (macOS ships it):
  ```sh
  sqlite3 .tmp/live/memory.db "SELECT key, length(body) FROM memory WHERE length(body) >= 1000 ORDER BY 2 DESC;"
  ```
- The `-wal`/`-shm` matter: without them you only see the last checkpoint, not recent rows.

---

## 10. Open questions / next steps

1. ~~Finish the run~~ — **DONE** (§7.2): all 43 run against {R2, R5}; shortlist = the 8-union.
2. *(Optional, not done)* run **R3** on the other 30 as a strictness-robustness probe.
3. **Get real ground truth:** James labels ~15–20 of the fat memories SPLIT/COHERE. Then compute precision/recall for R2 vs R5 against his labels — the only way to actually pick a default.
4. **Decide the bias setting** (R2 willing vs R5 conservative). Cost-of-error leans conservative: over-splitting fragments the `[[link]]` graph and spawns orphans (expensive to undo); under-splitting leaves a fat row (cheap to fix).
5. **Then build** (once a rubric + threshold is chosen): the size finder (`store::oversized` / `memory_big` MCP tool) + read-time hint on `memory_get`, with the chosen rubric text shipped as the judging guidance. Mirror the `dupes` plumbing (`Session::dupes`, positional wire, read-only annotation).

---

## 11. Run status (was: cold-start procedure)

**The run is COMPLETE (43/43). Nothing left to execute.**

Method notes, kept for any future re-run:
- One agent per (memory × rubric) via `spawn_agent`; verbatim prompts in §5.
- **Small batches (2–6 agents/turn) land reliably**; wide parallel batches (20+) were interrupted more than once.
- Record every verdict in `.tmp/split-exp/RESULTS.tsv` (cols: memory, B, R2, R3, R5, atoms).
- The corpus is regenerable from the live DB (§9).

To continue the *work* (not the test), see §10.

---

## Appendix A — raw SPLIT atom keys

**seaweedfs-ec-vacuum**
- B: `seaweedfs-ec-vacuum-decode-route`, `seaweedfs-ec-ratio-policy`
- R2: `seaweedfs-ec-vacuum-decode-route`, `seaweedfs-ec-vacuum-disklocation-gotcha`, `seaweedfs-ec-ratio-policy`
- R3: `seaweedfs-ec-vacuum`, `seaweedfs-ec-ratio-policy`
- R5: — (COHERE)

**network-gateway-dns**
- B: `network-gateway-ngf`, `network-dns`, `network-wireguard-vpn`, `network-firewall-ip-block`, `network-browser-pna`
- R2/R3: — (COHERE)
- R5: `network-gateway-ngf`, `network-dns-split-horizon`

**documents-ocis**
- B: `documents-ocis-deploy`, `documents-ocis-chart-publish`, `documents-ocis-storage`, `documents-ocis-oidc`, `documents-ocis-ldap`, `documents-onlyoffice-wopi`
- R2: `documents-ocis-deployment`, `documents-ocis-chart-ops`, `documents-ocis-storage`, `documents-ocis-auth-oidc-ldap`, `documents-ocis-onlyoffice-wopi`
- R3/R5: — (COHERE)

**identity-authentik**
- B: `identity-authentik-groups`, `identity-authentik-profile-mapping`, `identity-authentik-oidc-provider-module`, `identity-authentik-tokens`, `identity-authentik-outposts`
- R2/R3/R5: — (COHERE)

**ellmephant-mcp** — all four COHERE.

**seaweedfs-build-deploy** — all four COHERE.

**seaweedfs-open-work**
- B: `seaweedfs-ec-fullness-ramp-invariant`, `seaweedfs-ec-orphan-dat-file`, `seaweedfs-phantom-volume-entries`, `seaweedfs-chart-image-tag-drift`, `seaweedfs-ec-decode-vif-stale`, `seaweedfs-decode-operator-serialization`, `seaweedfs-admin-service-discovery`
- R2/R3/R5: — (COHERE)

**finance-robinhood-data-gotchas**
- B: `finance-robinhood-csv-parsing-gotchas`, `finance-robinhood-futures-pdf-blocks`, `finance-robinhood-transaction-codes`, `finance-robinhood-acat-basis`, `finance-robinhood-coverage-pnl`
- R2: `finance-robinhood-csv-parsing-gotchas`, `finance-robinhood-futures-pdf-blocks`, `finance-robinhood-txn-codes-and-basis`
- R3: — (COHERE)
- R5: `finance-robinhood-export-parsing`, `finance-robinhood-code-reference`, `finance-robinhood-export-coverage-status`

**finance-other-brokerage-stock-docs**
- B: `finance-other-brokerage-stock-docs`, `finance-crwd-acat-basis-gap`, `finance-pdf-extraction-no-ocr`
- R2/R3/R5: — (COHERE)

**ellmephant-helm-chart**
- B: `ellmephant-helm-chart`, `ellmephant-release-automation`
- R2/R3/R5: — (COHERE)

**ellmephant-write-policy**
- B: `ellmephant-write-policy`, `ellmephant-link-hygiene`
- R2/R3/R5: — (COHERE)

**cline-mcp-reconnect**
- B: `cline-mcp-reconnect`, `ellmephant-connection-timeouts`, `gateway-keepalive-timeout`
- R2/R3/R5: — (COHERE)

**finance-prior-years-reconstruction**
- B: `finance-tax-filing-status`, `finance-tax-yearly-agi-tax`, `finance-capital-loss-carryforward`, `finance-other-income-sources`
- R2/R3/R5: — (COHERE)

### Newly flagged rows 14–43 (rubrics {R2, R5} only)

**github-chart-release-pattern**
- R2: `github-chart-releaser-action`, `helm-oci-build-script`
- R5: — (COHERE)

**seaweedfs-fork**
- R2: `seaweedfs-fork-setup`, `seaweedfs-fork-merge-hygiene`, `seaweedfs-fork-docs`
- R5: — (COHERE)

**ellmephant-gotchas**
- R2: `ellmephant-vec0-gotchas`, `ellmephant-code-layout-gotchas`
- R5: `ellmephant-layout`, `ellmephant-sqlite-vec`

**cluster-diagnostics**
- R2: — (COHERE)
- R5: `cluster-metrics-diagnostics`, `cluster-flow-diagnostics`

*(All other rows 14–43: COHERE under both.)*

---

## Appendix B — files produced this session

| path | what |
|---|---|
| `SPLIT_EXPERIMENT.md` (repo root) | this document |
| `.tmp/split-exp/RESULTS.tsv` | machine ledger — all 43 verdicts |
| `.tmp/corpus/all.txt` | full 107-memory corpus (key/chars/lines/title + body), size-desc |
| `.tmp/live/memory.db` (+`-wal`,`-shm`) | throwaway snapshot of the live store |

_End of Part 1. Binary experiment complete (43/43) — see §7.2._

---

# Part 2 — the scoring era (same-day addendum)

Everything above is the **binary** run (SPLIT/COHERE). Everything below happened AFTER §11 and is NOT reflected in §7.2.

## 12. Binary R2 vs R5 — complementary, near-tie
On the 6 disagreements my read splits 3–3. R2 (willing) missed the clearest bag (`network-gateway-dns`); R5 (conservative) missed `documents-ocis`. Different blind spots; neither dominates. Verifier→R5, finder→R2, ship the union.

## 13. Graded 0–1 scoring (8 memories)
Bare-number **R2·score over-fires** — `identity-authentik` 0.85, `seaweedfs-open-work` 0.92; can't tell "cohesive+detailed" from "two topics". Bare-number **R5·score flatlines** (all ~0.05). **Their average is useless**: R5≈const → avg≈R2/2 → same order as R2, plus R2's over-eagerness.

## 14. Why R5 flatlined (ablation, 3 memories)
| variant | net | cluster | fork |
|---|---|---|---|
| R5 orig (bare #, "harm", conservative) | 0.10 | 0.05 | 0.00 |
| V1 drop conservatism | 0.55 | 0.20 | 0.20 |
| V2 conservative, FIXED anchor (0.5=borderline) | 0.15 | 0.15 | 0.10 |
| V3 allow ONE sentence of reasoning | 0.65 | 0.35 | 0.62 |
| V4 count intents (int) | 6 | 2 | 7 |

Root cause = **negative "harm" framing × number-only format (no thinking room)**. Either fix alone restores range; V2 proves the anchor was NOT the cause. V4 over-counts FACETS (fork=7).

## 15. SplitScore — THE WINNER (was R5v3)
Prompt: *"Decide if this memory mixes distinct retrieval intents that should live in separate memories. Write ONE sentence of reasoning, then a score 0.00–1.00 (0=one coherent topic, 1=clear grab-bag)."* Emit the SCORE; the CALLER thresholds (~0.4–0.5) — policy outside the prompt.

Top-10 (SplitScore / R2·score): documents-ocis 0.72/0.80 · seaweedfs-ec-vacuum 0.60/0.55 · finance-robinhood-data-gotchas 0.55/0.55 · network-gateway-dns 0.45/**0.85** · seaweedfs-open-work 0.40/**0.92** · seaweedfs-build-deploy 0.30/0.35 · finance-other-brokerage 0.30/0.30 · ellmephant-mcp 0.25/0.20 · identity-authentik 0.20/**0.85** · ellmephant-helm-chart 0.20/0.35.

→ SplitScore: **no clear false positives**; cost = rare false negative (`network` 0.45). R2·score FPs: `identity-authentik`, `open-work`. **Noisy** (`network` 0.65 one run, 0.45 another) → use self-consistency k=3.

## 16. Pre-filter = recall gate (cheap)
`size ≥~1000 chars` → `changed since last judged (doc_hash)` → SplitScore → threshold.
Signal check (flagged 8 vs other 35): size 1662/1332 = ONLY usable. Tag count 3.88/3.80 DEAD. visits 1.13/1.23 DEAD. times_useful 0.13/0.09 DEAD. out-links 2.25/2.71 weak+inverted; in-links 4.0/2.74 weak.

## 17. Bridge score — NEGATIVE RESULT
Neighbour-dispersion (k=5), meant to catch bags straddling communities. Pulled all 107 vectors from the vec0 shadow tables (`memory_vec_rowids` + `memory_vec_vector_chunks00`, substr 4096-byte slices; Ruby — the sqlite3 gem can't load vec0).
FLAGGED avg 0.4145 vs other 0.4280 (no separation, slightly inverted); FLAGGED avg rank **58/107** = coin flip. Tracks store SPARSITY, not intent. **Drop it.**

## 18. Tag audit (live store)
369 slots · 129 distinct tags · **81 (63%) singletons**. Near-dups: gotcha/gotchas, taxes/tax, kubernetes/k8s. Tags ~cosmetic (semantic-only search + duplicated by the key) → hygiene not correctness. → `todo-tag-audit`.

## 19. Store fixes made
- `todos` hub: pruned its STALE inline spoke list; fixed an ESCAPE BUG (7 literal `\n`, 0 real newlines → single-line blob; only 1 memory affected).
- created `todo-tag-audit`.

## 20. Memory-rules debate (UNRESOLVED)
74/107 bodies have no newline; 23 of 43 fat ones are single-line blobs (8–40 sentences). "Facts one per line" is ignored by ~85% and is cosmetic. Naive sentence count ~50% CONTAMINATED (`network` 40 dots→19; `ec-vacuum` 29→8). Recommendation: demote newlines to style; PRIMARY rule = "one TOPIC per memory"; don't reprocess; don't store derivable stats. Intents are JUDGED not counted; if stored, a side table keyed by doc_hash.

## 21. STATUS: PROPOSAL ONLY — nothing built
grep src/tests for `splitscore|memory_split|split-finder` = **0 hits**; MCP registers 10 tools, none is `memory_split`; schema has no judgment table. Only real artifacts = **this doc** + memories `[[ellmephant-split-finder]]` (design) & `[[todo-tag-audit]]`.
⚑ `.tmp/` is **GITIGNORED** — `RESULTS.tsv`, `tagfreq.txt`, `vecs.tsv` there are volatile. Essentials are inlined here.

---
_End of record — Part 2._

---

# Part 3 — the shipped v0 (write-time nudge), 2026-10-11

Rethink after Part 2: the whole experiment concluded *an LLM reading the prose is the only thing that works* — so why build a separate finder agent when the **writer is already an LLM holding the full body**? Demote the finder to the retroactive safety net; promote the authoring-time nudge to v0.

## 22. What shipped
- **`UPSERT_DESCRIPTION`** (src/mcp/mod.rs:63) now carries an *active pre-write check*: *"Before saving, check: does the body hold ≥2 distinct retrieval intents? Then upsert them as separate memories, one call each — never a misc/gotchas bucket; each atom must stand alone (≥2-3 facts)."* The passive "split distinct" was already there; this makes the writer **stop and evaluate**, and makes the *action* explicit (one `memory_upsert` call per atom — the tool writes exactly ONE memory).
- **Test pin** added (src/mcp/mod.rs, server-info test): upsert description contains "separate memories" — alongside the existing "one topic"/"merge" asserts.
- **Why the description, not `INSTRUCTIONS`:** Cline + TypingMind both drop server `instructions`; tool *descriptions* survive (`[[cline-drops-mcp-instructions]]`, `[[typingmind-connector-drops-mcp-instructions]]`).
- **Hygiene floor** (≥2-3 facts, standalone, no misc bucket) is lifted straight from R2/R5 — Part 2's rubric work feeds the wording.

## 23. Honest limits (why the finder is NOT dead)
- Advisory text — a fresh write can be waved through (the R1 frozen-rubber-stamp failure mode). **Must be measured**: does the writer actually split fat bodies?
- Covers **only** post-deploy LLM-agent writes. The existing fat rows (incl. the 8-union shortlist) are untouched; programmatic upserts are moot.
- **Next:** observe real upserts. If the nudge is ignored → that is the evidence that tips us back to the `memory_split` / `store::oversized` finder (§10.5). If obeyed → finder becomes the rare retroactive sweep only.
