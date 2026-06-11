# Latte: In-Memory Oracle Data Validation — Design Specification

**Status:** Draft for discussion
**Date:** 2026-06-10 (revised 2026-06-11 after review feedback in PR #180)
**Scope:** High-level architecture for an **in-memory oracle** in Latte: stateful, client-side tracking of mutation histories that enables complex data-corruption testing and tombstone-resurrection detection — without a secondary oracle database cluster, and without writing a dedicated validation workload script.

> **Naming note (review feedback):** Latte's existing validation (`data_validation.rn`) is already "oracle-free" — it recomputes expected data from the stress index. What this proposal *adds* is an in-memory oracle: compact client-side state that remembers what each tracked partition should contain after an arbitrary mutation history. The document was renamed accordingly.

---

## 1. Problem Statement

ScyllaDB correctness testing under fault injection (node kills, network partitions, repair/compaction races) requires knowing what data *should* be in the database, so that reads can be checked against it. Today this is done two ways:

1. **Physical oracle cluster (Gemini model).** Every mutation is sent to both the System Under Test (SUT) and a second, single-node "oracle" cluster. Precise, but expensive (a whole second cluster to provision and feed), slow, and prone to false positives from replication lag and oracle-side issues.
2. **Stateless re-generation (cassandra-stress model, current Latte `data_validation.rn`).** Data for row *i* is generated deterministically from *i*; on read-back the expected value is recomputed and compared. Zero storage cost, and Latte's Rune scripting can stretch it further — `context.data` can hold custom state and `elapsed_time` enables time-based expectations — so workloads that change their expectations over time *are* expressible today. But each such script reinvents its own ad-hoc bookkeeping, the Rune VM is interpreted and single-threaded per VM (per-row mutation tracking at scale is impractical there), and a failed comparison reports only "expected X, got Y" with no history of how the partition got into that state. Validating **interleaved mutation histories** (insert → update → delete → re-insert), and especially **tombstone resurrection**, needs structured, efficient, reusable state — exactly the scenarios where ScyllaDB bugs hide (lost writes, resurrection, repair drift).

**Goal:** give Latte a built-in validation engine that:

- adds a structured, native (Rust-side) **in-memory oracle** with a defined API, instead of per-script ad-hoc state in `context.data`;
- needs **no second database cluster** (the Gemini cost model) and **no dedicated validation Rune script** — a single configuration call in the workload's `prepare()` enables a self-driving background validator alongside the main workload;
- handles **mutation histories** per partition (insert → update → delete → re-insert → …);
- detects **tombstone resurrection** (deleted data coming back after `gc_grace_seconds` / repair anomalies);
- when corruption is found, produces the **full mutation timeline of the affected partition** so engineers can correlate it with what was happening on the SUT cluster at those times (compaction, repair, nemesis events);
- runs at Latte speed (thread-per-core, async, no blocking I/O in the hot path) and **reports itself in Latte's normal statistics** (throughput and latency per validation operation type);
- uses **process memory only** for the live tracking state in the first version, with a clean extension point for a disk-backed store (SQLite) later, for very long-running tests.

---

## 2. Core Idea: Static Memory Blocks and Pointer Tracking

### 2.1 The payload block

At startup, the validator allocates a single static block of high-entropy random bytes (configurable, typically 1–2 GB) in process memory. This block never changes for the duration of the run.

Every variable-length value (text, varchar, blob) that the validated workload writes to the SUT is **a slice of this block**, identified by a compact pointer — an `(offset, length)` pair. With a block ≤ 4 GB, both fit in 32-bit integers: **any value of any size is tracked in exactly 8 bytes**.

This decouples *physical data volume* from *logical correctness tracking*:

| Column | Stored payload (SUT) | Tracked locally | Saving |
|---|---|---|---|
| blob, ~200 B | 200 B | 8 B (offset + length) | 25× |
| varchar, ~100 B | 100 B | 8 B | 12.5× |
| double / int / timestamp | 8 B | 8 B (raw value, no pointer needed) | 1× |
| typical 1.2 KB row (4 text cols + numerics) | ~1224 B | ~56 B | ~22× |

10 million tracked live rows ≈ 560 MB of client RAM instead of a 12+ GB oracle database. Materializing a payload is a memcpy (slice the block); the payloads themselves are incompressible random data, but the block could also be constructed from compressible content (a text file).

### 2.2 The partition block: a static, indexed partition set

The set of validated partitions is **fixed and known upfront**: `partitions_count` (configured in the workload, §3.2) partition keys are generated at startup and stored in a dedicated **partition block**, each identified by its **index** `0 .. partitions_count-1`. The set never grows or shrinks during the run.

Everywhere else in the engine — row entries, mutation log records, validation checkpoints — a partition is referenced by **its index alone**: a single small integer instead of key bytes. Mapping an index back to the actual partition key (via the partition block) is needed only when issuing statements and, rarely, when producing a forensic report. All mutations and validation reads target only these partitions; inserts add rows to them, they are never "created" or "dropped" as tracked entities.

### 2.3 The pointer registry — one structure, two jobs

The in-memory **pointer registry** is the heart of the design. For each partition index it holds:

- rows in a structure **ordered by clustering key** (respecting the table's `CLUSTERING ORDER`), so expected ordering — and therefore range scans within a partition — fall out naturally; each row is just its clustering-key bytes plus per-column value pointers (numerics stored raw);
- a small amount of per-partition state: `deleted` marker (partition-level only, see below), `uncertain` flag (in-flight/timed-out mutation), last-validated time, and a `quarantined` flag for partitions that already failed validation (§5.2).

The registry drives **both sides** of the validated workload:

- **Write path.** The validator picks a partition index and an operation, allocates fresh slices of the payload block (or selects an existing row for updates), materializes key and values, and executes the insert/update/delete against the SUT. The registry entry is updated when the driver confirms the operation.
- **Validation path.** The validator picks a partition index, materializes the expected rows from the pointers (slicing the payload block), fetches the **whole actual partition** from the SUT, and compares byte-by-byte, in clustering order.

Because the same registry entry is the single source of truth for what was written and what is expected back, there is no separate "model" to keep in sync.

**Deletions — no per-row `deleted` flag:** since validation always fetches whole partitions, a confirmed **row delete simply removes the row entry** from the registry (its payload slices are recycled) — the expected partition contents shrink, and a resurrected row shows up as an unexpected extra row whose history the mutation log still carries. A confirmed **partition delete** removes all row entries and sets the partition's `deleted` marker: the partition stays in the set (it is just an index — costs nothing) and is validated for **emptiness**, covering tombstone resurrection long after `gc_grace_seconds`. A later insert into a deleted partition clears the marker and starts a new life of the partition — giving exactly the insert → delete → re-insert histories the engine is meant to exercise. `gc_grace_seconds` is read from the table schema and recorded with each deletion in the mutation log, so reports can state whether a resurrection occurred inside or outside the grace window.

### 2.4 Why not the alternatives

A comparison study ("Oracle-Free Precision Data Validation Architecture for ScyllaDB Stress Testing") and the PR #180 review discussion examined several designs:

- **cassandra-harry** makes every value *self-identifying*: values are produced by a reversible generator from a compact 64-bit ID, so a value read back can be decoded to the exact operation that wrote it. Elegant and zero-storage, but its full form assumes a strictly sequential operation stream replayed deterministically — a poor match for Latte's concurrent thread-per-core execution and for real runs where operations time out with unknown outcome. Reimplementing Harry's model checker in Rune is also a non-starter (interpreted, single-threaded per VM). One Harry idea is kept as an optional enhancement (§8): self-identifying payload headers.
- **Transparent interception ("piggyback") mode**: instead of driving its own statements, the engine could intercept the *main* workload's bound statements, sample partitions by pk-hash, and track 8-byte xxhash digests of the written values. Attractive because it validates the user's actual workload with zero script changes — but the oracle no longer controls payload bytes, so expected payloads cannot be materialized from RAM, full values must be written to the mutation log on disk (~100 GB/day at 1 kOps with 1.2 KB rows), and per-partition mutation ordering cannot be serialized client-side. Kept as a follow-on mode (§9); the pointer design remains the core.
- **Ad-hoc Rune state (`context.data`)**: possible today and useful for simple time-based expectation changes, but unstructured, per-script, interpreter-speed, and without forensics — see §1.

---

## 3. Enablement & Configuration: the `validation` Rune API

### 3.1 One config call, no dedicated script

No CLI flags and no validation workload script are required. The user's existing workload enables the feature with a single call in `prepare()`:

```rune
pub async fn prepare(db) {
    let v = validation::config("latte.validated");      // target keyspace.table
    v.oracle_memory  = 2 * validation::GB;              // registry cap → derives rate (§3.3)
    v.payload_memory = 1 * validation::GB;              // static random payload block size
    v.partitions_count = 10_000;                        // static validated partition set
    v.ratios = #{ insert: 50, update: 30, row_delete: 15, partition_delete: 5 };
    v.mutation_to_validation_ratio = 10;                // 1 validation read per 10 mutations
    db.configure_validation(v);
}
```

Fully automatic mode — defaults for everything except the target table:

```rune
db.configure_validation(validation::config("latte.validated"));
```

This fits Latte's existing extension points: native types and instance methods are registered to Rune the same way `Context` and its `execute_*` family already are (`src/scripting/mod.rs`, `#[rune::function(instance)]`), and `prepare()` runs once before worker threads are spawned, so the registered configuration is simply read by the engine after `prepare()` returns. Scripts can expose knobs through Latte's existing `-P` parameter mechanism (`latte::param!`), so SCT can override e.g. `validation_memory` per run without any new CLI surface. A thin CLI alias (e.g. `--validate-table ks.tbl` injecting a default config) is a possible later convenience for scripts that should stay untouched.

Validation requires a **time-based run duration** (`-d <time>`): the rate derivation in §3.3 needs a known wall-clock duration. Cycle-count-based runs are not supported in v1 — the engine fails fast at startup with a clear error.

### 3.2 Statements from schema introspection

The config deliberately does **not** ask the user to enumerate partition keys, clustering keys and column types: the engine introspects the target table from the driver's cluster metadata — key structure, column types, `CLUSTERING ORDER`, `gc_grace_seconds` — and generates the statement set itself:

- `INSERT` covering all key columns and the tracked regular columns;
- `UPDATE` of tracked regular columns by full primary key;
- `DELETE` of a row (full pk) and of a whole partition;
- `SELECT` of a full partition (the validation read).

Optional config fields refine this: `columns = [...]` restricts which regular columns are tracked; unsupported types in v1 cause a fail-fast error at startup unless excluded via `columns`. The mutation mix comes from `ratios`; validation reads are paced separately via `mutation_to_validation_ratio` (§5.2).

Configuration summary:

| Field | Default | Meaning |
|---|---|---|
| target (`config(...)` arg) | — (required) | `keyspace.table` to validate; schema introspected |
| `oracle_memory` | e.g. 1 GB | registry budget; drives derived mutation rate (§3.3) |
| `payload_memory` | e.g. 1 GB | static random payload block size |
| `partitions_count` | e.g. 10 000 | **static** validated partition set, generated at startup with indices `0..n-1`; all mutations/validations target these partitions (also the range-scan key set) |
| `ratios` | balanced default mix | insert / update / row_delete / partition_delete weights |
| `mutation_to_validation_ratio` | e.g. 10 | how many mutations per one validation read (§5.2) |
| `columns` | all supported regular columns | restrict tracked columns |
| `key_prefix` / `key_range` | engine default | key-space isolation when sharing the main workload's table (§5.3) |
| `rate` | derived (§3.3) | manual override of validated mutations/s |
| `max_errors` | e.g. 1 | validation failures tolerated before stopping the run (§5.2) |
| `read_selection` | round-robin | round-robin or random partition selection for validation reads |
| `client_timestamps` | on | client-supplied `USING TIMESTAMP` (opt-out) |

### 3.3 Memory budget drives validated throughput

The dominant constraint is client memory, so the primary knob is `oracle_memory`, not a rate. Accumulating state is the row entries — clustering-key bytes plus per-column pointers plus small metadata, referencing their partition by index (~56 B for a typical row; the static partition and payload blocks are fixed costs, allocated once). After schema introspection the engine knows the exact per-row footprint, so:

```
row_capacity         ≈ oracle_memory / bytes_per_row
derived_insert_rate  ≈ row_capacity / test_duration
derived_rate         ≈ derived_insert_rate / insert_share(ratios)
```

That is the whole mechanism — **no runtime memory accounting**. If the rate is right, the budget cannot be exceeded within the test duration; deletions only free memory along the way, adding safety margin (predicting exactly how much a row or partition delete frees is non-trivial — a partition may hold any number of rows — and v1 deliberately skips that correction; it only makes the estimate conservative). If the process nevertheless runs out of memory, that is a bug in the engine's accounting — and an OOM is exactly how we want to find out, not paper over it.

The expected envelope remains roughly 1–10 kOps/s of validated operations riding alongside a normal full-rate Latte workload.

### 3.4 Lifecycle and statistics

- **Lifecycle:** the config call in `prepare()` only registers configuration. The engine generates the partition set and allocates the blocks, then spawns the validator when the main benchmark phase starts (not during warmup, which would pre-fill the oracle and skew the derived rate) and stops it with the run. The validator runs as its own set of async streams with its **own rate limiter**, independent of the main workload's `-r` — reusing Latte's existing per-thread stream + rate-limiter machinery.
- **Statistics:** validator operations are recorded through Latte's existing per-function stats pipeline (`FnStats` → per-sample `cycle_latency_by_fn`), labeled `val::insert`, `val::update`, `val::row_delete`, `val::partition_delete`, `val::read`. Throughput and latency percentiles for the validation stream therefore appear in live samples and the final report exactly like user workload functions, with no new reporting machinery.
- **Compactness everywhere:** runtime tracking — registry rows, mutation log records, checkpoints — carries only the **partition index**, never partition key bytes. Indices are resolved to keys through the partition block only when statements are issued and when a forensic report is generated, which happens rarely (on a detected integrity error) and is typically followed by stopping the run.

---

## 4. The Mutation Log (forensics)

The registry answers "*what should this partition contain now?*". It deliberately does not answer "*how did it get into this state?*" — keeping full history in memory for every partition would defeat the memory budget. That job belongs to the **mutation log**.

The mutation log is an append-only record, written to a **file** on the client, of every operation the validator performs:

```
operation id | wall-clock timestamp (µs) | client write timestamp | op type (INSERT/UPDATE/DELETE/...)
| partition index | clustering key | column pointers / numeric values | outcome
```

Properties:

- **Write-only during normal operation.** Nothing on the hot path ever reads it. Entries are pushed to an async channel and a dedicated writer thread performs buffered sequential appends — no blocking I/O in Latte's executor threads, no measurable impact at validated-workload rates (1–10 kOps/s ≈ a few MB/s of sequential writes; entries are pointer-sized, ~50 B/op, with partitions referenced by index).
- **Read only when validation fails.** On a detected corruption, the engine extracts from the log **every mutation ever applied to the affected partition index, with timestamps**, and emits it as the partition's timeline — resolving the index to the real partition key via the partition block, and expected payload bytes for any historical operation by slicing the payload block with the logged pointers. Engineers then line this timeline up against SUT-side events — compaction runs, repairs, nemesis operations in SCT — to find what the cluster was doing when the data went wrong.
- **Validation checkpoints are logged too** — just `(partition index, time)`. A failure at time *t_fail* with the last checkpoint at *t_ok* bounds the investigation window to `(t_ok, t_fail]`; the forensic report is generated with that time marker and the timeline highlights mutations inside the window.

**File vs. SQLite:** for v1 a plain append-only file (binary or line-delimited) is the right choice — appends are the dominant operation and sequential file writes are the fastest and simplest path; the read side (one partition's history) is rare and can afford a filtered scan. SQLite becomes attractive when logs grow very large or get queried repeatedly (indexed lookup by partition, ad-hoc SQL during investigations). The log sits behind the same narrow storage interface as the registry (§7), so the backend can be swapped without touching the engine — file first, SQLite as a performance/ergonomics upgrade if scans prove too slow in practice.

---

## 5. Architecture Overview

```
+----------------------------------------------------------------------------------+
|                                LATTE CLIENT                                      |
|                                                                                  |
|  User's Rune workload script:                                                    |
|    prepare(db):  db.configure_validation(validation::config("ks.tbl"))           |
|    run(db, i):   main load generator (full rate, untracked keys)                 |
|         |                                                                        |
|         v                                                                        |
|  +--------------------------------------------------------------------------+   |
|  |                              RUST CORE                                   |   |
|  |                                                                          |   |
|  |  main workload streams        BACKGROUND VALIDATOR (own streams,         |   |
|  |  (rate: -r, stats: per fn)     own rate limiter, stats: val::*)          |   |
|  |                                   |                                      |   |
|  |  +-----------------+   +----------------------+   +--------------------+ |   |
|  |  | Payload block   |   | Pointer registry     |   | Validation engine  | |   |
|  |  | (1-2 GB random  |<--| (in-memory, sharded  |-->| pick partition idx,| |   |
|  |  |  bytes)         |   |  per core)           |   | materialize        | |   |
|  |  | slice = payload |   | partition idx        |   | expected, read SUT,| |   |
|  |  +-----------------+   |  -> ordered rows     |   | compare, classify  | |   |
|  |  +-----------------+   |  -> value pointers   |   +--------------------+ |   |
|  |  | Partition block |<--| + deleted/uncertain/ |              | (on fail) |   |
|  |  | (idx -> pk,     |   |   quarantined        |              v           |   |
|  |  |  static set)    |   +----------------------+   +--------------------+ |   |
|  |  +-----------------+           | (async channel)  | Forensic reporter  | |   |
|  |                        +----------------------+   | partition timeline | |   |
|  |                        | Mutation log writer  |-->| + anomaly report   | |   |
|  |                        | (dedicated thread,   |   +--------------------+ |   |
|  |                        |  buffered appends)   |                          |   |
|  |                        +----------------------+                          |   |
|  +-----|--------------------------------------------------------------------+   |
|        |        \--> mutation_log file (append-only; SQLite optional)           |
|        v   ScyllaDB Rust driver (async)                                         |
+--------|--------------------------------------------------------------------- --+
         v
    ScyllaDB SUT  (same table also receiving untracked high-throughput load)
```

### 5.1 Write path

1. The validator's scheduler picks the next mutation per the configured `ratios` and a target partition index.
2. The engine allocates payload-block slices (or selects an existing row for updates), materializes the partition key (from the partition block) and values, and executes the generated statement against the SUT.
3. Mutations carry a **client-supplied write timestamp** set at the CQL-protocol level. This is **enabled by default and optional** (`client_timestamps = false`), mirroring Gemini's behavior — monotonic client timestamps make last-write-wins resolution fully deterministic.
4. On confirmation: registry updated — row entry created or repointed; row delete removes the row entry (slices recycled); partition delete clears the partition's rows and sets its `deleted` marker (§2.3). Mutation log entry queued.
5. On **timeout (outcome unknown)**: the partition is marked *uncertain* and the log records the attempt; validation of that partition then accepts either outcome until a subsequent confirmed operation resolves the state. This is the largest source of false positives in oracle-based setups and is handled explicitly from day one.

To keep ordering unambiguous, the validator maintains **at most one in-flight mutation per partition** (cheap to enforce in the registry; the validated stream runs at 1–10 kOps/s across thousands of partitions, so this costs no throughput).

### 5.2 Validation path

Validation reads are paced by **`mutation_to_validation_ratio`** — one full-partition validation read per *N* mutations (default e.g. 10). This keeps the trade-off explicit: validate frequently enough to catch corruption with a tight investigation window, but without adding significant read load to the cluster on top of the main workload. With the derived mutation rate, the resulting read rate is known upfront and reported in stats as `val::read`.

1. The engine selects the next partition index — round-robin (even coverage, oldest-validated first) or random (better at catching time-correlated bugs), per `read_selection`. Quarantined partitions are skipped (see below).
2. Expected rows are materialized from registry pointers; the actual partition is fetched **whole** from the SUT. Reads use Latte's **configured consistency level** — the user picks what fits the scenario (e.g. quorum is the sensible default when nemeses are running).
3. Byte-by-byte comparison **in clustering order** — the ordered registry validates row ordering too, and range scans within a partition (clustering-range slices) can be validated against ordered registry slices. The static partition set is exactly the key set available for such scans.
4. Discrepancies are classified per the taxonomy (§6). On a mismatch, returned bytes are also checked against the partition's *historical* pointers from the mutation log, distinguishing "stale/resurrected value from operation N" from "bytes from no known write".
5. **Success** → checkpoint `(partition index, time)` logged, last-validated time updated. **Failure** → the forensic reporter generates the anomaly report with the failure time marker and the partition's full timeline from the mutation log, and the partition is **quarantined**: excluded from all future validation reads, so one corrupted partition produces exactly one error and one report, not a repeating stream of them.
6. **Error budget:** `max_errors` (default 1) controls how many validation failures the run tolerates before Latte stops. The usual mode for fault-injection testing is fail-fast — corruption found, full forensics produced, run ends — but a higher budget allows surveying how widespread the damage is.

### 5.3 Co-location with the main workload

By default the validator targets the **same table** as the main workload — validated data must experience the same compaction, compression, repair and fault pressure as everything else; that co-location is the point. Tracked partitions must then never be touched by the untracked load generator, or the registry diverges from reality and produces false alarms. Isolation is enforced by the engine, not by convention:

- text partition keys: validator prepends a reserved prefix (`key_prefix`, default e.g. `val:`);
- numeric partition keys: validator owns a reserved range (`key_range`, default e.g. negative keys).

The main workload simply avoids that prefix/range (Latte's generators already produce non-negative hashes, so numeric defaults usually need no script change). A dedicated table is also possible — just point the config at one — at the cost of less shared pressure.

### 5.4 Scale and rate: why one instance is enough (review discussion)

The validated subset is deliberately small (memory-bounded, 1–10 kOps/s) while the workload as a whole runs at full Latte rate. This is sufficient for fault detection because nemeses do not target individual rows: they destroy or corrupt whole sstables, nodes, or time windows. Any such event that touches data will, with high probability, touch tracked partitions — and the *untracked* full-rate load is what creates the compactions, large sstables and repair pressure that make those events dangerous. In other words: the untracked load broadens the blast area, the tracked sample detects the damage.

Latte is also frugal on memory/CPU, so the feature can ride along most existing workloads without bigger instances or extra loaders.

Running multiple Latte instances against the same table needs no coordination for correctness — each validator owns its own key prefix/range (instance id folded into `key_prefix`/`key_range`), so tracked sets are disjoint by construction. Explicit splitting of the tracked key space across instances to scale validated throughput beyond one machine's memory is the natural phase-2 extension; it is listed in §9.

---

## 6. Anomaly Taxonomy (what the engine detects)

| # | Anomaly | How detected | Forensics produced |
|---|---------|--------------|---------------------|
| 1 | **Lost write / lost row** | Expected row or cell absent from SUT | Partition timeline; op that wrote the missing data, with timestamp |
| 2 | **Stale value / lost update** | Returned bytes match an older pointer state from the partition's history, not the current registry state | Timeline shows the update that should have won and when |
| 3 | **Tombstone resurrection (partition)** | Partition marked `deleted` in registry, but SUT returns rows | Deletion timestamp vs. `gc_grace_seconds`; full pre-deletion history from the log |
| 4 | **Tombstone resurrection (row / cell)** | Unexpected row or cell content whose bytes match historical pointers from the mutation log — history proves it existed and was deleted/overwritten | As above, per row/cell |
| 5 | **Byte-level corruption** | Returned value matches no expected slice of the payload block | Expected vs. actual payload dump; timeline for correlation with SUT events |
| 6 | **Phantom rows** | Rows present that no logged operation ever created (no match in mutation history) | Timeline proves no such write; payload captured |
| 7 | **Row-count / shape / ordering mismatch** | Partition cardinality or clustering order differs from registry | Missing/extra/misplaced clustering keys listed |

Rows 3–4 are the headline capability: stateless generation cannot detect them at all, and the physical-oracle approach reports them only as opaque diffs. Here every resurrection report carries the deletion time, the data's original write times, and the bounded investigation window — directly matchable against SUT cluster logs. The mutation log is what tells resurrection (row 4) apart from phantoms (row 6): same symptom — an unexpected row — but only one has a history.

---

## 7. Storage Backends: Memory Now, SQLite Later

Two state stores exist in the design, each behind a deliberately narrow interface so backends can be added without touching the engine:

| Store | v1 backend | Later add-on | Why later helps |
|---|---|---|---|
| **Pointer registry** (live expected state) | In-memory, sharded per core (no cross-core locking on hot path) | SQLite-backed (or hybrid spill) | Long-running tests whose tracked-row set outgrows RAM; resuming tracking after a planned client restart |
| **Mutation log** (history) | Append-only file, async writer thread | SQLite (WAL mode, async writer) | Indexed per-partition history lookup on very large logs; ad-hoc SQL during investigations |

Benchmarks from the comparison study support this split: concurrent in-memory maps sustain 500 kOps/s+ (vs. the 1–10 kOps/s the validated workload needs), while SQLite in WAL mode with an async writer reaches 30–35 kOps/s on NVMe — fine as a future backend, unnecessary as a v1 dependency.

The v1 implementation makes exactly two concessions to that future: the trait boundaries, and registry/log entries being plain serializable structs.

---

## 8. Optional Enhancement (future): Self-Identifying Payloads

Borrowed from cassandra-harry: prefix each generated payload with a small fixed header encoding the operation id that produced it (the rest of the payload remains a payload-block slice). Costs a few bytes per value and one extra comparison step; buys **provenance of wrong data** — when validation finds unexpected bytes, the header (if intact and known) identifies exactly which historical operation the data came from, distinguishing "resurrected value from op #4711, deleted 40 minutes ago" from "bytes from another partition" from "garbage from no known write". Cleanly additive: changes payload generation only, nothing in the registry or engine. Proposed as a fast-follow, not v1.

---

## 9. What the First Version Includes (and excludes)

**In scope (v1):**

- `validation` Rune module: `validation::config("ks.table")` + `db.configure_validation(v)` in `prepare()` — **no dedicated validation script, no new CLI flags** (per-run overrides via the existing `-P` mechanism). Time-based run duration (`-d <time>`) required; cycle-count runs fail fast.
- Schema introspection (keys, column types, `CLUSTERING ORDER`, `gc_grace_seconds`) and automatic statement generation; fail-fast on unsupported types unless excluded via `columns`.
- Static partition set (`partitions_count`, indexed `0..n-1`, keys in a dedicated partition block); all runtime tracking references partitions by index only.
- Memory-budget configuration (`oracle_memory`) with the derived mutation rate of §3.3 — no runtime memory accounting; an overrun is an engine bug surfacing as OOM, by design.
- Static payload block + pointer registry (in-memory, per-core sharding, clustering-ordered rows), covering common CQL scalar types: text/varchar/blob via pointers; numerics/uuid/timestamp stored raw.
- Insert / update / row delete / partition delete flows with uncertainty (timeout) handling, client write timestamps (default-on, opt-out), one in-flight mutation per partition; row deletes remove registry entries (no per-row flags), partition deletes set the partition-level `deleted` marker with emptiness validation through the resurrection window.
- Background validator streams with their own rate limiter, started with the main benchmark phase; operations reported through Latte's per-function stats as `val::insert` / `val::update` / `val::row_delete` / `val::partition_delete` / `val::read` (throughput + latency percentiles in samples and final report).
- Validation pacing via `mutation_to_validation_ratio`; round-robin and random partition selection; full-partition reads at the configured consistency level; ordering checks; full anomaly taxonomy of §6; quarantine of failed partitions and `max_errors` stop budget.
- Engine-enforced key-space isolation (`key_prefix` / `key_range`) for same-table co-location with the main workload.
- Mutation log to append-only file with async writer (partition-index records, `(partition index, time)` checkpoints); forensic timeline extraction on failure.
- Structured anomaly reports (human- and machine-readable) wired into Latte's existing reporting.

**Out of scope (v1), natural follow-ons:**

- CLI convenience alias (e.g. `--validate-table ks.tbl`) injecting a default config for unmodified scripts.
- Transparent interception ("piggyback") mode validating the main workload's own partitions via digest tracking (§2.4).
- Multi-instance splitting of the tracked key space (scale validated throughput beyond one client's memory, §5.4).
- Clustering-range-scan and cross-partition (token-range) scan validation over the tracked set (the ordered registry already provides the expected ordering).
- SQLite backends for registry and/or mutation log (long-running tests, restart survival, SQL forensics).
- Self-identifying payload headers (§8).
- Range deletes and TTL-aware validation; counters; LWT; cycle-count-based runs.
- Materialized-view / secondary-index consistency checks (same registry, different read paths).

---

## 10. Risks & Open Questions (for discussion)

1. **Memory ceiling.** Memory-only registry caps tracked rows at roughly (RAM budget ÷ ~56 B per row) — ~18 M rows/GB. Sufficient for fault-injection scenarios (which need depth of history per partition more than raw partition count)? If not, SQLite spill moves up the priority list.
2. **Timeout semantics.** Is accept-either-outcome for uncertain ops enough, or add an immediate read-back-and-resolve step after each timed-out write to collapse ambiguity sooner?
3. **Collections/UDTs in v1?** The plan above limits v1 to scalar types (text/varchar/blob via pointers, numerics/uuid/timestamp raw) and fails fast otherwise. Do we want collection and UDT support already in v1 — at the cost of a more complex registry model (per-element tracking, element-level tombstones) — or is scalar coverage enough for the first iteration?

---

## 11. Summary

One config call in the workload's `prepare()` — `db.configure_validation(validation::config("ks.table"))` — starts a self-driving background validator next to the main workload: statements are generated from introspected schema, the mutation mix comes from configured ratios, validation reads are paced by `mutation_to_validation_ratio`, and the mutation rate is derived upfront from the memory budget and run duration — no runtime accounting. The oracle is three compact structures: a static random **payload block** making every value a reproducible 8-byte pointer; a static indexed **partition block** so all tracking references partitions by a single integer; and the **pointer registry** holding each partition's expected rows in clustering order — simultaneously the workload driver and the oracle, with row deletes simply removing entries and partition deletes validated for emptiness through the resurrection window. A **mutation log file** — append-only, off the hot path — records every operation with timestamps, so any detected corruption comes with the partition's full mutation timeline (failed partitions are quarantined after one report; `max_errors` bounds the run). Every validation operation reports throughput and latency through Latte's normal per-function statistics (`val::*`). Both stores hide behind narrow interfaces: memory and flat file now, SQLite later for long-running tests.
