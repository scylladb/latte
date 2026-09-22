# Feasibility: In-Memory Partition Oracle in Latte

## What this is

`oracle_design.md` describes an in-memory "oracle" that mirrors every
`INSERT`/`UPDATE`/`DELETE` into a compact model (checksums + metadata, never raw values) and
checks SUT reads against it — catching value corruption, missing rows, and tombstone
resurrection without a second cluster. This document answers **two questions**: is it feasible
to integrate into Latte, and what would the Rune-facing API look like.

Verdict: **feasible.** It fits Latte's architecture cleanly. Three capabilities Latte lacks
today must be added first (§3); the rest reuses existing machinery.

---

## 1. Why it fits Latte

| Oracle need | Latte already has |
|---|---|
| Shared state across all workers | `Arc` fields on `Context` (`statements`, `stats`, `partition_row_presets`) survive the per-worker deep-clone — proven channel for an `Arc<OracleManager>` |
| Per-mutation hook | scripts already call `db.execute*` per mutation; add the oracle call alongside |
| Read rows to validate | `execute_prepared_with_result` already returns rows to Rune as objects |
| Value checksums | `serialize.rs` (Rune→`CqlValue`→wire) and `deserialize.rs` (wire→Rune) give canonical bytes to hash |
| Failure reporting | existing `--validation-strategy` (fail-fast/retry/ignore) + `signal_failure` |
| Bounded validated subset | scripts already gate on `latte::hash*` keys — sample on partition idx |
| Per-partition identity & expected size | `get_partition_info` already yields a stable `idx` (0..total_partitions) and `rows_num` |

The oracle **engine** ships as a separate crate (pure LWW/liveness over `ck:u64` + `crc16:u16` —
direct port of the pseudocode); the engine is API-agnostic, but **this design targets the CQL API
only**. Latte adds thin glue: the partition store, a checksum codec, and the Rune methods. Support
for the DynamoDB/Alternator API is future work — see §6.

---

## 2. Where oracles live — hang them off `get_partition_info`

The oracle attaches to Latte's **existing partition machinery** rather than a parallel pk map.
`get_partition_info(preset, idx)` already returns a `Partition` with a **stable logical index**
`idx ∈ 0..total_partitions` and the **expected** `rows_num` for it. That `idx` is the natural
oracle key (bounded and stable, unlike arbitrary pk values), and `rows_num` feeds the
`missing`/completeness check and pre-sizing for free. So:

- `Partition` becomes a **handle**: it carries `idx`, `rows_num`, and an
  `Arc<Mutex<PartitionOracle>>` for that idx, fetched-or-created from a shared store on access.
- Oracle methods live **on the partition handle**: `partition.oracle_save(...)`,
  `partition.validate(rows, complete)`, `partition.drop()`. The partition you already look up
  *is* the thing you validate.

**Storage & the clone pitfall.** The per-idx oracles live in a store shared like
`partition_row_presets` — an `Arc` field on `Context` so it survives the per-worker deep-clone
and every worker sees the same oracle for a given idx:
```
oracle_store: Arc<HashMap<(table, preset, idx) -> Mutex<PartitionOracle>>>   // sharded
```
It must **not** be embedded in `RowDistributionPreset`, because `_get_partition_info` does
`preset.cloned()` on every call — that would deep-copy the whole oracle map per lookup. Keep it
a separate `Arc`; `get_partition_info` reads the (cloned) preset for the math, then fetches the
oracle `Arc<Mutex<..>>` from the shared store. (Consequence: `Partition` loses `#[rune(copy)]`
— it now holds an `Arc`, so it's clone-by-Arc, not copy. Existing scripts using `partition.idx`
/ `partition.rows_num` are unaffected.)

**Concurrency.** Any worker can touch any partition (cycles are pulled round-robin), so each
`PartitionOracle` sits behind its own `Mutex`; methods lock only that one idx. Sound because
`_resolve` resolves LWW by **timestamp, not arrival order** (§3) — interleaved appends from
different workers replay correctly given monotonic timestamps. Contention is negligible across a
few-thousand-partition sample.

**One contract for authors:** a whole-partition validation (`complete = true`, the `missing`
sweep) is only logically correct if no other task is concurrently writing that idx — validate in
a phase/function separate from the writers. Per-row checks (value mismatch, resurrection) are
always safe alongside writes.

---

## 3. Required enablers (don't exist in Latte today)

1. **Client-side microsecond timestamps.** LWW correctness needs the exact write timestamp.
   Add `latte::next_timestamp()` (global `AtomicU64`, `next = max(now_us, prev+1)` → strictly
   monotonic, so equal-microsecond conflicts can never arise). Scripts bind it into
   `USING TIMESTAMP :ts` and pass the same value to the oracle.
2. **Write status mapping.** Reuse `cass_error.rs`: `Ok ⇒ EXECUTED`; timeout / post-dispatch
   error ⇒ `POTENTIALLY_EXECUTED`; pre-dispatch error ⇒ `FAILED`. The oracle handles
   `POTENTIALLY_EXECUTED` conservatively (never a false positive).
3. **Checksum round-trip** — the main technical risk: `crc(written) == crc(read-back)` must
   hold. Hashing Rune values is fragile (`deserialize.rs` returns uuid→String, timestamp→i64,
   blob→Vec). **Hash canonical CQL wire bytes on both paths instead**: on write via
   `serialize.rs::to_scylla_value`, on read by crc-ing the `FrameSlice` bytes already present in
   `deserialize.rs` (on the fetch thread, as the doc intends). Frozen/non-frozen collections and
   counters are deferred until round-trip is verified per type.

Everything else is a straight port of the pseudocode.

---

## 4. Proposed Rune API

Oracle access is **partition-handle-centric**: look the partition up with `get_partition_info`,
then call oracle methods on the returned handle. Two layers, as requested: **primitives** (script
orchestrates ts + status) and a **combined** helper built on them (auto ts + status, less misuse).

### Setup — once in `prepare()`
```rune
db.init_partition_row_distribution_preset("main", ROW_COUNT, ROWS_PER_PART, "100:1").await?;
db.oracle_init("users", "main", ["pk"], ["ck"], ["name", "email"], #{
    ttl_grace_secs: 5,
    memory_limit: "512MiB",   // 0 / absent = unlimited
}).await?;
```
`oracle_init` binds the oracle to a preset and registers the table's column layout (clustering
keys vs value columns) + codec.

### Get the handle (write or read path)
```rune
let p = db.get_partition_info("main", idx).await;   // p.idx (stable), p.rows_num (expected)
let pk = hash(p.idx);                                // script maps logical idx -> CQL pk
```

### Write — Layer A (primitives, on the handle)
```rune
let ts = latte::next_timestamp();
let status = match db.execute_prepared("ins", [pk, ck, name, email, ts]).await {
    Ok(_)  => oracle::EXECUTED,
    Err(e) => if e.is_timeout() { oracle::POTENTIALLY_EXECUTED } else { return Err(e) },
};
// columns object: present key = written (() = explicit NULL); absent key = not written
p.oracle_save("users", [ck], #{name: name, email: email}, oracle::INSERT, 0 /*ttl*/, ts, status);
```

### Write — Layer B (combined: auto ts + status)
```rune
db.execute_oracle("ins", #{
    partition: p, table: "users", pk: pk, ck: [ck],
    columns: #{name: name, email: email},
    op: oracle::INSERT, ttl: 0,
}).await?;   // injects USING TIMESTAMP, executes, maps status, records on p's oracle — one call
```

### Update / delete
```rune
// UPDATE: only touched columns; untouched columns keep prior history
db.execute_oracle("upd_email", #{partition:p, table:"users", pk:pk, ck:[ck],
                                 columns:#{email:new_email}, op:oracle::UPDATE}).await?;
// row DELETE
db.execute_oracle("del_row", #{partition:p, table:"users", pk:pk, ck:[ck], op:oracle::DELETE}).await?;
// partition DELETE
let ts = latte::next_timestamp();
db.execute_prepared("del_part", [pk, ts]).await?;
p.oracle_delete_partition("users", ts);   // oracle kept → resurrection still detectable
```

### Validate — read path (on the handle)
```rune
let rows = db.execute_prepared_with_result("sel_partition", [pk]).await?;
let errors = p.validate("users", rows, true).await?;  // complete = whole-partition
// p.rows_num is the expected count → reinforces the `missing` sweep
// errors: Vec of #{ck, kind, detail, mutation_log}
// kind ∈ "value_mismatch" | "missing" | "resurrected" | "unexpected_row" | "ck_collision"
// ck_collision (two SUT rows share one CRC-64 ck → partition untrustworthy) ⇒ drop it
```
Returned errors route through Latte's existing `--validation-strategy` (the wrapper signals each
like `signal_failure`), so fail-fast/retry/ignore govern behavior with no new plumbing. Use
`complete = false` for clustering-key-narrowed reads (skips only the "missing" sweep).

### Memory
```rune
p.oracle_drop("users");   // only if script is truly done with this partition (rare;
                          // oracles are normally kept all run — see §5)
db.oracle_size();         // current footprint (bytes), budgeting aid
```

### Putting it together
```rune
const SAMPLE = latte::param!("validate_every", 100);   // validate ~1/100 of partitions

pub async fn run(db, i) {
    let p = db.get_partition_info("main", i).await;
    let pk = hash(p.idx);
    let ck = hash2(i, 1);
    let name = text(i, 16);
    if p.idx % SAMPLE == 0 {                            // validated subset → mirror into oracle
        db.execute_oracle("ins", #{partition:p, table:"users", pk:pk, ck:[ck],
                                   columns:#{name:name, email:()}, op:oracle::INSERT}).await?;
    } else {
        db.execute_prepared("ins", [pk, ck, name, (), latte::next_timestamp()]).await?;
    }
    Ok(())
}

pub async fn validate(db, i) {                          // run as a separate -f function / phase
    let p = db.get_partition_info("main", i).await;
    if p.idx % SAMPLE == 0 {
        let rows = db.execute_prepared_with_result("sel_partition", [hash(p.idx)]).await?;
        p.validate("users", rows, true).await?;
    }
    Ok(())
}
```

---

## 5. Memory behavior (how it stays bounded)

Each `save` **appends** a record, so footprint is `Σ mutations × record_bytes(n)`
(`record_bytes(n) = 11 + 2n`, + ~24 B per new row: 8 B CRC-64 `ck` + ~16 B bookkeeping) —
**not** fixed by the sample size. A hot
partition updated many times grows without bound. The sample gate caps the *number* of
partitions; it does **not** cap a single partition's growth. So a hard memory limit with active
**eviction** is required, or a long run OOMs.

- **Oracles are kept for the whole run.** resurrection detection requires the post-delete history to stay resident, and a
  partition is re-read/re-validated across cycles.
  `p.oracle_drop` remains available for scripts that genuinely finish with a
  partition.
- **Eviction under pressure.** Manager tracks a running byte total against `memory_limit`. On
  breach it **drops whole partition oracles** to get back under budget (not "stop adding new" —
  that does nothing about an already-huge partition). Eviction is the only real reclamation
  lever given append-only growth. 
- **Eviction loses that partition's validation** — and should not be created again.
- A partition is dropped (and its bytes reclaimed) when it becomes untrustworthy: `save` raises
  **`OracleError`** (now only an equal-microsecond same-cell conflict), or `validate` reports
  **`ck_collision`** (two SUT rows share one CRC-64 ck — vanishingly rare, see
  `oracle_design.md` § *Collision probability*). Workload continues.

Note the budget should be sized for the *mutation volume on the sampled partitions over the run
length*, not just `partitions × rows`.

---

## 6. DynamoDB / Alternator applicability (out of scope for now)

This design targets the **CQL API only**. The oracle *engine* (mutation log, LWW replay,
liveness verdict, checksum compare) is API-agnostic; the CQL coupling is confined to four
swappable adapters. Supporting Alternator means re-implementing these, not redesigning the
engine — deferred as future work:

| Adapter | CQL today | Alternator later |
|---|---|---|
| **Serialization codec** | CRC over canonical CQL wire bytes | CRC over canonical AttributeValue JSON. Engine only needs `crc(written) == crc(read-back)`; it never inspects bytes. |
| **Write ordering** | client `USING TIMESTAMP` (exact LWW key) | Alternator has no client timestamp and no read-back. Order by a logical sequence from `save()` under `always_use_lwt` + the single-writer-per-partition rule, or simply avoid re-mutating a row within a short window so server wall-clock timestamps stay unambiguous. |
| **Liveness rules** | row marker: `INSERT` marks, `UPDATE` doesn't | simpler — `PutItem` replaces, `UpdateItem` upserts, `DeleteItem` removes. Validate observable liveness (what Query/GetItem returns); internal markers don't matter. |
| **TTL model** | cell-level, coordinator-clock + TTL | opt-out (no TTL → grace machinery inert), or item-level with a client-written absolute expiry and grace ≥ the Alternator reclamation period (default 24h). |

Only write ordering is real work, and it rides on the single-writer constraint the design
already mandates.
