# Latte: Oracle-Free Data Validation — Design Specification

**Status:** Draft for discussion
**Date:** 2026-06-10
**Scope:** High-level architecture for client-side data validation in Latte, enabling complex data-corruption testing scenarios and tombstone resurrection detection without a secondary "oracle" database cluster.

---

## 1. Problem Statement

ScyllaDB correctness testing under fault injection (node kills, network partitions, repair/compaction races) requires knowing what data *should* be in the database, so that reads can be checked against it. Today this is done two ways, both unsatisfactory:

1. **Physical oracle cluster (Gemini model).** Every mutation is sent to both the System Under Test (SUT) and a second, single-node "oracle" cluster. Precise, but expensive (a whole second cluster to provision and feed), slow, and prone to false positives from replication lag and oracle-side issues.
2. **Stateless re-generation (cassandra-stress model, current Latte `data_validation.rn`).** Data for row *i* is generated deterministically from *i*; on read-back the expected value is recomputed and compared. Zero storage cost, but fundamentally limited: it only works when each row is written once and never changed. It cannot validate **updates, overwrites, deletes, range deletes, or any interleaved mutation history** — exactly the scenarios where ScyllaDB bugs hide (lost writes, tombstone resurrection, repair drift).

**Goal:** give Latte a built-in, client-side validation engine that:

- needs **no second database cluster**;
- handles **mutation histories** per partition (insert → update → delete → re-insert → …);
- detects **tombstone resurrection** (deleted data coming back after `gc_grace_seconds` / repair anomalies);
- when corruption is found, produces the **full mutation timeline of the affected partition** so engineers can correlate it with what was happening on the SUT cluster at those times (compaction, repair, nemesis events);
- runs at Latte speed (thread-per-core, async, no blocking I/O in the hot path);
- uses **process memory only** for the live tracking state in the first version, with a clean extension point for a disk-backed store (SQLite) later, for very long-running tests.

---

## 2. Core Idea: the Static Memory Block and Pointer Tracking

### 2.1 The memory block

At startup, Latte allocates a single static block of high-entropy random bytes (configurable, typically 1–2 GB) in process memory. This block never changes for the duration of the run.

Every variable-length value (text, varchar, blob) that the validated workload writes to the SUT is **a slice of this block**, identified by a compact pointer — an `(offset, length)` pair. With a block ≤ 4 GB, both fit in 32-bit integers: **any value of any size is tracked in exactly 8 bytes**.

This decouples *physical data volume* from *logical correctness tracking*:

| Column | Stored payload | Tracked locally | Saving |
|---|---|---|---|
| blob, ~200 B | 200 B | 8 B (offset + length) | 25× |
| varchar, ~100 B | 100 B | 8 B | 12.5× |
| double / int / timestamp | 8 B | 8 B (raw value, no pointer needed) | 1× |
| typical 1.2 KB row (4 text cols + numerics) | ~1224 B | ~56 B | ~22× |

10 million tracked live rows ≈ 560 MB of client RAM instead of a 12+ GB oracle database. Materializing a payload is a memcpy (slice the block); the payloads themselves are incompressible random data, but it could be constructed also from compressible one (text file).

### 2.2 The pointer registry — one structure, two jobs

The in-memory **pointer registry** is the heart of the design. It holds, for every tracked partition, the pointers describing its expected contents (partition key, clustering keys, per-column value pointers, numeric values stored raw, plus a small amount of state such as a deleted flag and last-validated time).

The registry drives **both sides** of the workload:

- **Write path.** Latte picks a pointer set (allocating fresh slices of the block, or selecting an existing entry for updates), converts it to a partition key and row values, and executes the insert/update/delete against the SUT. The registry entry is updated when the driver confirms the operation.
- **Validation path.** Latte picks a tracked partition from the registry (round-robin or random), materializes the expected rows from the pointers (slicing the memory block), fetches the actual partition from the SUT, and compares byte-by-byte.

Because the same registry entry is the single source of truth for what was written and what is expected back, there is no separate "model" to keep in sync.

Deleted partitions are not removed from the registry: they are flagged `deleted` and **kept (and still validated) at least until `gc_grace_seconds` plus a safety margin has elapsed** — that is precisely the window in which tombstone resurrection can occur, and a validation read of a deleted partition must return zero rows.

### 2.3 Why not the alternatives

A comparison study ("Oracle-Free Precision Data Validation Architecture for ScyllaDB Stress Testing") also examined cassandra-harry. Harry makes every value *self-identifying*: values are produced by a reversible generator from a compact 64-bit ID, so a value read back from the database can be decoded back to the exact operation that wrote it (Harry calls these generators "invertible bijections" — functions that run forwards, ID → value, and backwards, value → ID). It is an elegant zero-storage model, but its full form assumes a strictly sequential operation stream replayed deterministically — a poor match for Latte's concurrent thread-per-core execution and for real runs where some operations time out with unknown outcome. Reimplementing Harry's model checker in Rune is also a non-starter (interpreted, single-threaded per VM).

The pointer-tracking design borrows what matters from both worlds: cassandra-stress-like cheap deterministic materialization, plus stateful tracking that handles updates and deletes. One Harry idea is worth keeping on the table as an **optional enhancement** (§7): embedding a small self-identifying header in each payload, so that when wrong bytes come back we can tell *which* operation originally produced them, not only that they are wrong.

---

## 3. The Mutation Log (forensics)

The registry answers "*what should this partition contain now?*". It deliberately does not answer "*how did it get into this state?*" — keeping full history in memory for every partition would defeat the memory budget. That job belongs to the **mutation log**.

The mutation log is an append-only record, written to a **file** on the client, of every operation the validated workload performs:

```
operation id | wall-clock timestamp (µs) | op type (INSERT/UPDATE/DELETE/...)
| partition key | clustering key | column pointers / numeric values | outcome
```

Properties:

- **Write-only during normal operation.** Nothing on the hot path ever reads it. Entries are pushed to an async channel and a dedicated writer thread performs buffered sequential appends — no blocking I/O in Latte's executor threads, no measurable impact at validated-workload rates (limited to 1–10 kOps/s ≈ a few MB/s of sequential writes).
- **Read only when validation fails.** On a detected corruption, the engine extracts from the log **every mutation ever applied to the affected partition, with timestamps**, and emits it as the partition's timeline. Engineers then line this timeline up against SUT-side events — compaction runs, repairs, nemesis operations in SCT — to find what the cluster was doing when the data went wrong.
- **Validation checkpoints are logged too.** Each successful validation of a partition appends a checkpoint record. A failure at time *t_fail* with the last checkpoint at *t_ok* bounds the investigation window to `(t_ok, t_fail]`; the timeline highlights mutations inside that window.

**File vs. SQLite:** for v1 a plain append-only file (binary or line-delimited) is the right choice — appends are the dominant operation and sequential file writes are the fastest and simplest path; the read side (one partition's history) is rare and can afford a filtered scan. SQLite becomes attractive when logs grow very large or get queried repeatedly (indexed lookup by partition key, ad-hoc SQL during investigations). The log sits behind the same narrow storage interface as the registry (§6), so the backend can be swapped without touching the engine — file first, SQLite as a performance/ergonomics upgrade if scans prove too slow in practice.

---

## 4. Architecture Overview

```
+----------------------------------------------------------------------------------+
|                                LATTE CLIENT                                      |
|                                                                                  |
|  Rune workload scripts (thin):                                                   |
|    main load generator (untracked keys)   validation workload (tracked keys)     |
|         |                                      |                                 |
|         |                  latte::tracker_* bindings                             |
|         v                                      v                                 |
|  +--------------------------------------------------------------------------+    |
|  |                              RUST CORE                                    |   |
|  |                                                                           |   |
|  |  +-----------------+   +----------------------+   +--------------------+  |   |
|  |  | Static memory   |   | Pointer registry     |   | Validation engine  |  |   |
|  |  | block (1-2 GB   |<--| (in-memory, sharded  |-->| pick partition,    |  |   |
|  |  | random bytes)   |   |  per core)           |   | materialize        |  |   |
|  |  |                 |   | partition -> pointers|   | expected, read SUT,|  |   |
|  |  | slice = payload |   | + deleted flag       |   | compare, classify  |  |   |
|  |  +-----------------+   +----------------------+   +--------------------+  |   |
|  |            \                    |                          |              |   |
|  |             \                   v (async channel)          v (on failure) |   |
|  |              \          +----------------------+   +--------------------+ |   |
|  |               \         | Mutation log writer  |   | Forensic reporter  | |   |
|  |                \        | (dedicated thread,   |-->| partition timeline | |   |
|  |                 \       |  buffered appends)   |   | + anomaly report   | |   |
|  |                  \      +----------------------+   +--------------------+ |   |
|  +-------------------\------------------------------------------------------+    |
|         |             \--> mutation_log file (append-only; SQLite optional)      |
|         v   ScyllaDB Rust driver (async)                                         |
+---------|------------------------------------------------------------------------+
          v
     ScyllaDB SUT  (also receiving untracked high-throughput load on same table)
```

### 4.1 Write path

1. Validation workload requests next operation (insert / update / delete — mix controlled by script parameters).
2. Rust core picks or allocates pointers, materializes partition key and values from the memory block.
3. Statement executes against the SUT.
4. On confirmation: registry entry created/updated/flagged-deleted; mutation log entry queued.
5. On **timeout (outcome unknown)**: the registry entry is marked *uncertain* and the log records the attempt; validation of that partition then accepts either outcome until a subsequent confirmed operation resolves the state. This is the largest source of false positives in oracle-based setups and is handled explicitly from day one.

To keep ordering unambiguous, the validated workload maintains **at most one in-flight mutation per tracked partition** (cheap to enforce in the registry; the validated stream runs at 1–10 kOps/s across many partitions, so this costs no throughput). Optionally, mutations carry client-supplied `USING TIMESTAMP` so last-write-wins resolution is fully predictable.

### 4.2 Validation path

1. Engine selects partitions round-robin or random (round-robin = even coverage, oldest-validated first; random = better at catching time-correlated bugs; both supported, script-selectable).
2. Expected rows materialized from registry pointers; actual partition fetched from SUT.
3. Byte-by-byte comparison; discrepancies classified per the taxonomy (§5).
4. Success → checkpoint logged, `last_validated` updated. Failure → forensic reporter pulls the partition's full timeline from the mutation log and emits a structured anomaly report.

### 4.3 Workload isolation

Tracked partitions must never be touched by the untracked high-throughput load generator, or the registry diverges from reality and produces false alarms. Both workloads share the same physical table (so validated data experiences the same compaction/repair/fault pressure — that co-location is the point), and the key spaces are split by convention:

- text keys: reserved prefix for validated keys (e.g. `val_`);
- numeric keys: range split (e.g. validated workload owns negative keys, bulk load owns positive).

### 4.4 Rune integration

Scripts stay thin; all bookkeeping is native Rust exposed as bindings, roughly:

- `tracker::insert(db, ...)/update/delete` — pick pointers, execute, register, log;
- `tracker::validate_next(db, n)` / `tracker::validate_random(db, n)` — run validation rounds, return anomaly reports;
- configuration: block size, mix ratios, selection policy, gc_grace retention.

---

## 5. Anomaly Taxonomy (what the engine detects)

| # | Anomaly | How detected | Forensics produced |
|---|---------|--------------|---------------------|
| 1 | **Lost write / lost row** | Expected row or cell absent from SUT | Partition timeline; op that wrote the missing data, with timestamp |
| 2 | **Stale value / lost update** | Returned bytes match an older pointer state, not current registry state | Timeline shows the update that should have won and when |
| 3 | **Tombstone resurrection (partition/row)** | Partition flagged `deleted` in registry, but SUT returns rows | Deletion timestamp vs. `gc_grace_seconds`; full pre-deletion history from the log |
| 4 | **Cell-level resurrection** | Deleted/overwritten column reappears with old content | As above, per cell |
| 5 | **Byte-level corruption** | Returned value matches no expected slice of the memory block | Expected vs. actual payload dump; timeline for correlation with SUT events |
| 6 | **Phantom rows** | Rows present that the registry never created | Timeline proves no such write; payload captured |
| 7 | **Row-count / shape mismatch** | Partition cardinality differs from registry | Missing/extra clustering keys listed |

Rows 3–4 are the headline capability: stateless generation cannot detect them at all, and the physical-oracle approach reports them only as opaque diffs. Here every resurrection report carries the deletion time, the data's original write times, and the bounded investigation window — directly matchable against SUT cluster logs.

---

## 6. Storage Backends: Memory Now, SQLite Later

Two state stores exist in the design, each behind a deliberately narrow interface so backends can be added without touching the engine:

| Store | v1 backend | Later add-on | Why later helps |
|---|---|---|---|
| **Pointer registry** (live expected state) | In-memory, sharded per core (no cross-core locking on hot path) | SQLite-backed (or hybrid spill) | Long-running tests whose tracked-row set outgrows RAM; resuming tracking after a planned client restart |
| **Mutation log** (history) | Append-only file, async writer thread | SQLite (WAL mode, async writer) | Indexed per-partition history lookup on very large logs; ad-hoc SQL during investigations |

Benchmarks from the comparison study support this split: concurrent in-memory maps sustain 500 kOps/s+ (vs. the 1–10 kOps/s the validated workload needs), while SQLite in WAL mode with an async writer reaches 30–35 kOps/s on NVMe — fine as a future backend, unnecessary as a v1 dependency.

The v1 implementation makes exactly two concessions to that future: the trait boundaries, and registry/log entries being plain serializable structs.

---

## 7. Optional Enhancement (future): Self-Identifying Payloads

Borrowed from cassandra-harry: prefix each generated payload with a small fixed header encoding the operation id that produced it (the rest of the payload remains a memory-block slice). Costs a few bytes per value and one extra comparison step; buys **provenance of wrong data** — when validation finds unexpected bytes, the header (if intact and known) identifies exactly which historical operation the data came from, distinguishing "resurrected value from op #4711, deleted 40 minutes ago" from "bytes from another partition" from "garbage from no known write". Cleanly additive: changes payload generation only, nothing in the registry or engine. Proposed as a fast-follow, not v1.

---

## 8. What the First Version Includes (and excludes)

**In scope (v1):**

- Static memory block + pointer registry (in-memory, per-core sharding), covering common CQL types: text/varchar/blob via pointers; numerics/uuid/timestamp stored raw.
- Insert / update / row delete / partition delete flows with uncertainty (timeout) handling and `deleted`-flag retention through `gc_grace_seconds`.
- Validation engine with round-robin and random selection, full anomaly taxonomy of §5.
- Mutation log to append-only file with async writer; forensic timeline extraction on failure.
- Rune bindings + one reference workload (evolution of `data_validation.rn`) running mixed mutate/validate cycles alongside an untracked bulk load.
- Structured anomaly reports (human- and machine-readable) wired into Latte's existing reporting.

**Out of scope (v1), natural follow-ons:**

- SQLite backends for registry and/or mutation log (long-running tests, restart survival, SQL forensics).
- Self-identifying payload headers (§7).
- Range deletes and TTL-aware validation; collections/UDTs; counters; LWT.
- Multi-client coordination (several Latte instances sharing one tracked key space).
- Materialized-view / secondary-index consistency checks (same registry, different read paths).

---

## 9. Risks & Open Questions (for discussion)

1. **Memory ceiling.** Memory-only registry caps tracked rows at roughly (RAM budget ÷ ~56 B per row) — ~18 M rows/GB. Sufficient for fault-injection scenarios (which need depth of history per partition more than raw partition count)? If not, SQLite spill moves up the priority list.
2. **Timeout semantics.** Is accept-either-outcome for uncertain ops enough, or add an immediate read-back-and-resolve step after each timed-out write to collapse ambiguity sooner?
3. **gc_grace retention cost.** Delete-heavy profiles with large `gc_grace_seconds` accumulate flagged-deleted entries. Mitigate via test profiles with reduced `gc_grace_seconds` (standard practice in resurrection testing) or a cap on retained deletions?
4. **Validation read consistency.** `CONSISTENCY ALL` is the cleanest check but can mask single-replica anomalies; per-replica targeted reads find more bugs but need more machinery. Proposal: `ALL`/quorum in v1, per-replica mode later.
5. **Client-supplied timestamps.** `USING TIMESTAMP` makes the model deterministic but slightly diverges from default production write paths. Make it optional?
6. **Mutation log growth.** Append-only file grows unbounded over multi-day runs (modest: ~50 B/op ≈ 4 GB/day at 1 kOps). Rotation + retiring segments older than the oldest unvalidated state, or switch that store to SQLite with pruning?

---

## 10. Summary

One static random memory block makes every payload a cheap, reproducible 8-byte pointer. An in-memory **pointer registry** built on it is simultaneously the workload driver (pick pointer → derive keys/values → write) and the oracle (pick partition → materialize expected → compare with SUT), including deleted partitions retained through the resurrection window. A **mutation log file** — append-only, off the hot path — records every operation with timestamps, so any detected corruption comes with the partition's full mutation timeline, ready to be correlated with compaction/repair/nemesis events on the SUT cluster. Both stores hide behind narrow interfaces: memory and flat file now, SQLite later for long-running tests. All bookkeeping is native Rust; Rune scripts remain thin orchestration.
