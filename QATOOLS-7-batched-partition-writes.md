# QATOOLS-7 — Single-partition batched writes

## Goal

Let a workload write LOGGED batches in which every statement targets the **same
partition**, so Scylla skips the batchlog, while keeping the partition-size skew
control that `init_partition_row_distribution_preset` provides.

The dataset produced must be **identical** to the one a row-at-a-time workload
produces with the same preset, so read/validation functions keep using
`get_partition_info(preset, idx)` with no extra parameters.

---

## 1. Public API

Two new rune functions. No existing signature changes.

### 1.1 `db.set_partition_batch_size(preset_name, batch_size)`

Called in `prepare()` after `init_partition_row_distribution_preset`.

* Stores `batch_size` on the preset.
* Computes and caches `total_batches` (§3.3).
* Errors if `batch_size == 0` or the preset does not exist.
* Prints an info line in the style of the existing one:

```
info: set_partition_batch_size: preset_name=main, batch_size=10, total_batches=29768883
```

`total_batches` is the number of cycles needed to cover the dataset exactly
once. Intended workflow: run the workload once with any duration, read this line from
the output, then put the number into the test config / CI job as `--duration`
for subsequent populate runs. Other durations stay valid — see §1.3.

### 1.2 `db.get_partition_batch(preset_name, i) -> PartitionBatch`

`i` is latte's cycle index, passed verbatim. Returns:

```rust
#[derive(Any)]
pub struct PartitionBatch {
    #[rune(get, copy)] pub idx: u64,        // partition index
    #[rune(get)]       pub rows: Vec<u64>,  // global row indices, all in `idx`
}
```

* `rows.len()` is `min(batch_size, remaining rows in the partition)` — never 0.
* `i` enumerates every batch of the dataset; `i >= total_batches` wraps modulo
  `total_batches`.
* Errors if `set_partition_batch_size` was not called for this preset.

### 1.3 Any `--duration` must work

`--duration` accepts either a cycle count or a time span (`Interval::Count` /
`Interval::Time`). Both must be valid for a batched workload, and neither may
require the operator to know `total_batches` in advance:

* `i` is reduced modulo `total_batches` before use, so **any** cycle index is
  legal — there is no upper bound the operator can exceed and no error state.
* Running longer than `total_batches` cycles re-writes rows already written.
  Writes are idempotent upserts, so the dataset stays correct; the only cost is
  repeated work.
* Running fewer than `total_batches` cycles covers a prefix of the dataset.
  Every partition touched is written correctly, but coverage is partial.

This gives three usable modes, none of which needs special handling in the
implementation:

| intent | `--duration` | outcome |
|---|---|---|
| populate exactly once | `total_batches` | full coverage, no rewrites |
| time-boxed throughput run | e.g. `30m` | correct data, partial or repeated coverage |
| time-boxed populate | generous time span | full coverage once wrapped at least once |

Only the first mode needs the printed `total_batches`. The implementation must
not reject, warn about, or special-case the other two.

### 1.4 Usage

```rune
pub async fn prepare(db) {
    db.init_partition_row_distribution_preset(
        "main", ROW_COUNT, ROWS_PER_PARTITION, PARTITION_SIZES).await?;
    db.set_partition_batch_size("main", BATCH_SIZE).await?;
    db.prepare("insert", INSERT_CQL).await?;
}

pub async fn mywrite(db, i) {
    let b = db.get_partition_batch("main", i).await;
    let pk = hash(b.idx);
    let stmts = [];
    let rows = [];
    for idx in b.rows {
        stmts.push("insert");
        rows.push((pk, hash(idx), text(idx, 32)));
    }
    db.batch_prepared(stmts, rows).await?
}
```

---

## 2. Structural facts the implementation relies on

All are properties of the existing code in `src/scripting/row_distribution.rs`:

1. **Partition indices are contiguous per group.** The peel loop accumulates
   `partn_offset += current_partn_count`, so group `g` owns the index range
   `[Σ_{j<g} n_partitions_j, Σ_{j<=g} n_partitions_j)`.
2. **Every partition inside a group has the same `n_rows_per_partition`.**
   Therefore "batches per partition" is constant within a group.
3. **Partitions inside a group are selected round-robin**: the returned index is
   `partn_offset + k % n_partitions`, where `k` is the group-local row counter.
   Consequently partition `p`'s row of rank `r` sits at group-local
   `k = r * n_partitions + lane`, where `lane = p - partn_offset`.
4. **The group-local → global mapping is a fixed cycle pattern**: `C1` cycles of
   `[L1 rows of this group][R1 rows of the others]`, followed by `C2` cycles of
   `[L2][R2]`. This is invertible in closed form (§3.1–3.2).

No per-partition table is built or stored at any point. The preset must stay
`O(n_groups)` in memory.

---

## 3. Internals to implement

Add to `src/scripting/row_distribution.rs`.

### 3.0 Per-group cycle parameters

From `row_distributions[g] = (type1, type2)`:

```rust
struct GroupCycles { l1: u64, r1: u64, c1: u64, l2: u64, r2: u64 }
// l1 = type1.n_rows_for_left,  r1 = type1.n_rows_for_right, c1 = type1.n_cycles
// l2 = type2.n_rows_for_left,  r2 = type2.n_rows_for_right
```

### 3.1 `k_to_local` — group-local rank → that group's coordinate space

```rust
fn k_to_local(g: &GroupCycles, k: u64) -> u64 {
    let t1 = g.c1 * g.l1;
    if g.l1 > 0 && k < t1 {
        (k / g.l1) * (g.l1 + g.r1) + k % g.l1
    } else {
        let kk = k - t1;
        g.c1 * (g.l1 + g.r1) + (kk / g.l2) * (g.l2 + g.r2) + kk % g.l2
    }
}
```

### 3.2 `lift_one` / `lift` — re-insert preceding groups' rows

`lift_one` maps a position in the coordinate space *after* group `j`'s rows were
removed back into the space *before* their removal:

```rust
fn lift_one(g: &GroupCycles, m: u64) -> u64 {
    let t1 = g.c1 * g.r1;
    if g.r1 > 0 && m < t1 {
        (m / g.r1) * (g.l1 + g.r1) + g.l1 + m % g.r1
    } else {
        let mm = m - t1;
        g.c1 * (g.l1 + g.r1) + (mm / g.r2) * (g.l2 + g.r2) + g.l2 + mm % g.r2
    }
}

fn lift(preset: &RowDistributionPreset, gi: usize, k: u64) -> u64 {
    let mut cur = k_to_local(&cycles(preset, gi), k);
    for j in (0..gi).rev() {
        cur = lift_one(&cycles(preset, j), cur);
    }
    cur
}
```

`lift(gi, k)` is the global row index of group `gi`'s group-local rank `k`.
Cost: `O(n_groups)`, two divmods per group, no allocation.

Guard against `l2 == 0` / `r2 == 0` before the division in the `else` branches
(reachable only when a group has no type-2 cycles; assert or return early).

### 3.3 `total_batches`

```rust
fn total_batches(preset: &RowDistributionPreset, b: u64) -> u64 {
    preset.partition_groups.iter()
        .map(|g| g.n_partitions * g.n_rows_per_partition.div_ceil(b))
        .sum()
}
```

Cache this on the preset in `set_partition_batch_size`.

### 3.4 `get_partition_batch`

```rust
fn get_partition_batch(preset: &RowDistributionPreset, i: u64, b: u64)
    -> (u64, Vec<u64>)
{
    let mut c = i % preset.total_batches;
    let mut pref = 0u64;                                  // partition-index base of group
    for (gi, g) in preset.partition_groups.iter().enumerate() {
        let per  = g.n_rows_per_partition.div_ceil(b);    // batches per partition
        let span = g.n_partitions * per;                  // cycles owned by this group
        if c < span {
            let lane = c % g.n_partitions;                // which partition in the group
            let off  = (c / g.n_partitions) * b;          // first row rank of this batch
            let take = min(b, g.n_rows_per_partition - off);
            let rows = (0..take)
                .map(|t| lift(preset, gi, (off + t) * g.n_partitions + lane))
                .collect();
            return (pref + lane, rows);
        }
        c -= span;
        pref += g.n_partitions;
    }
    unreachable!()
}
```

Ordering is deliberate: `lane` advances every cycle and `off` advances only
after a full sweep of the group's partitions, so consecutive cycles land on
different partitions (different replica sets) rather than filling one partition
at a time.

### 3.5 Preset storage

Add to `RowDistributionPreset`:

```rust
pub batch_size:    u64,   // 0 = unset
pub total_batches: u64,   // cached; valid when batch_size != 0
```

Keep the struct `O(n_groups)`. Do not materialise any partition→size or
index→partition table.

---

## 4. Tests

Add to the existing `mod tests`. Use these presets, which cover
evenly-divisible, ragged, multi-group and fractional-multiplier cases:

| preset | init args | resulting groups |
|---|---|---|
| even | `1000, 25, "100:1"` | `40×25` |
| ragged | `1000, 13, "100:1"` | `76×13, 1×12` |
| skewed | `10000, 20, "80:1,15:2,5:4"` | `310×20, 57×40, 19×80` |
| fractional | `10000, 10, "49.1:1,49:2,1.9:2.5"` | `322×20, 326×10, 12×25` |

Batch sizes to exercise per preset: `1, 2, 4, 5, 6, 7, 13, 100` (include values
that do and do not divide the partition sizes, and values larger than them).

Assertions:

1. **`lift` agrees with the shipped mapping.** For each group `gi` and every
   `k` in `0..n_rows_per_group`, assert
   `get_partition_info(lift(gi, k)).0 == partn_offset(gi) + k % n_partitions`.
   This is the primary correctness gate — the existing peel loop is the oracle.
2. **Exact coverage.** Iterating `i` over `0..total_batches` and concatenating
   all `rows` must yield every index in `0..total_rows` exactly once — no gaps,
   no duplicates.
3. **Per-partition exactness.** Each partition must receive exactly its own
   `n_rows_per_partition` rows, not approximately.
4. **Batch invariants.** For every `i`: `rows` is non-empty; every element maps
   back to the returned partition via `get_partition_info`; no duplicate inside
   one batch; `rows.len() == min(batch_size, n_rows_per_partition - off)`.
5. **Wraparound.** `get_partition_batch(i + total_batches) ==
   get_partition_batch(i)`.
6. **`batch_size = 1` degenerate case.** `total_batches == total_rows` and every
   batch holds exactly one row.
7. **Existing tests must pass unmodified** — they are the regression gate for
   `init_partition_row_distribution_preset` and `get_partition_info`.

---

## 5. Integration test (runs on every PR)

Add a workload script plus a test case to the existing suite
(`tests/integration_test.rs`, executed by `.github/workflows/integration-test.yml`
via `cargo test --test integration_test -- --ignored --nocapture` against a
3-node ScyllaDB container cluster).

### 5.1 Workload — `workloads/batch_partition_validation.rn`

The point of this workload is the **cross-check**: data is written through the
new batched path and then validated through the *existing* row-granular path.
If the batching algorithm places a single row in the wrong partition, drops one,
or writes one twice, the validation functions fail.

```rune
// Writes single-partition LOGGED batches via the partition-row-distribution
// preset, then validates the result through the existing row-granular API.
//
// USAGE:
// 1) Create schema:
// $ latte schema workloads/batch_partition_validation.rn 172.17.0.2
//
// 2) Populate DB with batches ('-d' = the 'total_batches' value printed by
//    'set_partition_batch_size'):
// $ latte run workloads/batch_partition_validation.rn -q \
//     -d 600 -P row_count=1000 -P rows_per_partition=5 \
//     -P partition_sizes="\"100:1\"" -P batch_size=2 \
//     -f insert_batch -- 172.17.0.2
//
// 3) Validate row counts per partition:
// $ latte run workloads/batch_partition_validation.rn -q \
//     -d 1000 -P row_count=1000 -P rows_per_partition=5 \
//     -P partition_sizes="\"100:1\"" -P batch_size=2 \
//     -f get_many -- 172.17.0.2

use latte::*;

const ROW_COUNT = latte::param!("row_count", 1000);
const REPLICATION_FACTOR = latte::param!("replication_factor", 3);
const ROWS_PER_PARTITION = latte::param!("rows_per_partition", 5);
const PARTITION_SIZES = latte::param!("partition_sizes", "100:1");
const BATCH_SIZE = latte::param!("batch_size", 2);

const KEYSPACE = "latte";
const TABLE = "batch_validation";

const P_STMT = #{
    "INSERT": #{
        "NAME": "p_stmt_batch__insert",
        "CQL": `INSERT INTO ${KEYSPACE}.${TABLE}(pk, ck) VALUES (:pk, :ck)`,
    },
    "GET_MANY": #{
        "NAME": "p_stmt_batch__get_many",
        "CQL": `SELECT pk, ck FROM ${KEYSPACE}.${TABLE} WHERE pk = :pk LIMIT :max_limit`,
    },
    "GET_BY_CK": #{
        "NAME": "p_stmt_batch__get_by_ck",
        "CQL": `SELECT pk, ck FROM ${KEYSPACE}.${TABLE} WHERE pk = :pk AND ck = :ck`,
    },
    "COUNT": #{
        "NAME": "p_stmt_batch__count",
        "CQL": `SELECT COUNT(*) FROM ${KEYSPACE}.${TABLE} WHERE pk = :pk`,
    },
};

pub async fn schema(db) {
    db.execute(`CREATE KEYSPACE IF NOT EXISTS ${KEYSPACE} WITH REPLICATION = {
        'class': 'NetworkTopologyStrategy', 'replication_factor': ${REPLICATION_FACTOR} }`).await?;

    db.execute(`CREATE TABLE IF NOT EXISTS ${KEYSPACE}.${TABLE}(
        pk bigint,
        ck bigint,
        PRIMARY KEY (pk, ck)
    ) WITH CLUSTERING ORDER BY (ck ASC)`).await?;
}

pub async fn erase(db) {
    db.execute(`TRUNCATE TABLE ${KEYSPACE}.${TABLE}`).await?
}

pub async fn prepare(db) {
    db.init_partition_row_distribution_preset(
        "main", ROW_COUNT, ROWS_PER_PARTITION, PARTITION_SIZES,
    ).await?;
    db.set_partition_batch_size("main", BATCH_SIZE).await?;
    db.prepare(P_STMT.INSERT.NAME, P_STMT.INSERT.CQL).await?;
    db.prepare(P_STMT.GET_MANY.NAME, P_STMT.GET_MANY.CQL).await?;
    db.prepare(P_STMT.GET_BY_CK.NAME, P_STMT.GET_BY_CK.CQL).await?;
    db.prepare(P_STMT.COUNT.NAME, P_STMT.COUNT.CQL).await?;
}

// Write path: one cycle == one single-partition LOGGED batch.
pub async fn insert_batch(db, i) {
    let b = db.get_partition_batch("main", i).await;
    let pk = hash(b.idx);
    let stmts = [];
    let rows = [];
    for idx in b.rows {
        stmts.push(P_STMT.INSERT.NAME);
        rows.push([pk, hash(idx)]);
    }
    db.batch_prepared(stmts, rows).await?
}

// Validation: every partition must hold exactly the row count the preset
// declares. Fails if the batch path mis-distributed, dropped or duplicated rows.
pub async fn get_many(db, i) {
    let idx = i % ROW_COUNT;
    let partition = db.get_partition_info("main", idx).await;
    let pk = hash(partition.idx);
    let max_limit = partition.rows_num + 10;
    let custom_err = `partition ${partition.idx}: expected ${partition.rows_num} row(s)`;
    let validation_args = [partition.rows_num, partition.rows_num, custom_err];
    db.execute_prepared_with_validation(
        P_STMT.GET_MANY.NAME, [pk, max_limit], validation_args).await?
}

// Validation: the exact (pk, ck) pair the row-granular API expects for a given
// global row index must exist. Fails if the batch path wrote a row into the
// wrong partition or under the wrong clustering key.
pub async fn get_by_ck(db, i) {
    let idx = i % ROW_COUNT;
    let partition = db.get_partition_info("main", idx).await;
    let pk = hash(partition.idx);
    let ck = hash(idx);
    let custom_err = `missing row pk=${pk} ck=${ck} (global idx ${idx})`;
    let validation_args = [1, 1, custom_err];
    db.execute_prepared_with_validation(
        P_STMT.GET_BY_CK.NAME, [pk, ck], validation_args).await?
}

// Validation: server-side count per partition.
pub async fn count(db, i) {
    let idx = i % ROW_COUNT;
    let partition = db.get_partition_info("main", idx).await;
    let pk = hash(partition.idx);
    let custom_err = `partition ${partition.idx}: expected count ${partition.rows_num}`;
    let validation_args = [partition.rows_num, partition.rows_num, custom_err];
    db.execute_prepared_with_validation(
        P_STMT.COUNT.NAME, [pk], validation_args).await?
}
```

### 5.2 Test case — `tests/integration_test.rs`

Follow the shape of `test_latte_cql_data_validation_workload`. Do **not**
hardcode `total_batches`: parse it from the line printed by
`set_partition_batch_size` during `prepare()`, so the test stays correct when
the preset arithmetic changes. This mirrors the operator workflow from §1.1 —
run once, read the number, use it.

```rust
/// Parses "total_batches=<N>" out of the set_partition_batch_size info line.
fn parse_total_batches(result: &CommandResult) -> u64 {
    result
        .output
        .lines()
        .find(|l| l.contains("set_partition_batch_size:"))
        .and_then(|l| l.split("total_batches=").nth(1))
        .and_then(|v| v.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            panic!("no total_batches in latte output:\n{}", result.output)
        })
}

#[tokio::test]
#[ignore]
async fn test_latte_cql_batch_partition_workload() {
    let db = start_scylla().await.expect("Failed to start ScyllaDB");

    let latte = LatteVariant::Cql;
    let workload = workload_path("batch_partition_validation.rn");

    // Two presets: evenly divisible, and skewed with a ragged remainder.
    let cases: &[(&[&str], &str)] = &[
        (
            &["-P", "row_count=1000", "-P", "rows_per_partition=5",
              "-P", "partition_sizes=100:1", "-P", "batch_size=2"],
            "even",
        ),
        (
            &["-P", "row_count=1000", "-P", "rows_per_partition=2",
              "-P", "partition_sizes=50:1,30:2,20:5", "-P", "batch_size=3"],
            "skewed-ragged",
        ),
    ];

    let mut rf_args: Vec<&str> = Vec::new();
    if db._container.is_some() {
        rf_args.extend(["-P", "replication_factor=1"]);
    }

    for (params, label) in cases {
        println!("\n[TEST-INFO] === case: {label} ===");
        let mut args: Vec<&str> = params.to_vec();
        args.extend(rf_args.iter().copied());

        println!("[TEST-INFO] Phase 1: schema + truncate");
        latte.schema(&db, &workload, &args);
        let mut erase = args.clone();
        erase.push("-f=erase");
        latte.run(&db, &workload, "1", &erase);

        println!("[TEST-INFO] Phase 2: discover total_batches");
        let mut probe = args.clone();
        probe.push("-f=insert_batch");
        let probe_result = latte.run(&db, &workload, "1", &probe);
        assert_latte_success(&probe_result);
        let total_batches = parse_total_batches(&probe_result);
        println!("[TEST-INFO] total_batches = {total_batches}");

        println!("[TEST-INFO] Phase 3: populate via batches");
        let mut populate = args.clone();
        populate.push("-f=insert_batch");
        let populate_result =
            latte.run(&db, &workload, &total_batches.to_string(), &populate);
        assert_latte_success(&populate_result);
        assert_no_errors(&populate_result);
        assert_has_throughput_metrics(&populate_result);

        // Phases 4-6 validate through the EXISTING row-granular API. They are
        // the real assertions: they fail if a single row landed in the wrong
        // partition, was dropped, or was written twice.
        for func in ["get_many", "get_by_ck", "count"] {
            println!("[TEST-INFO] Phase: validate via {func}");
            let mut v = args.clone();
            let f = format!("-f={func}");
            v.push(&f);
            let r = latte.run(&db, &workload, "1000", &v);
            assert_latte_success(&r);
            assert_no_errors(&r);
        }

        println!("[TEST-INFO] Phase: time-based duration must be accepted");
        let mut timed = args.clone();
        timed.push("-f=insert_batch");
        let timed_result = latte.run(&db, &workload, "5s", &timed);
        assert_latte_success(&timed_result);
        assert_no_errors(&timed_result);

        println!("[TEST-INFO] Phase: over-running must stay correct");
        let mut over = args.clone();
        over.push("-f=insert_batch");
        let over_result =
            latte.run(&db, &workload, &(total_batches * 3).to_string(), &over);
        assert_latte_success(&over_result);
        assert_no_errors(&over_result);
        // data must still validate after wrapping several times
        let mut v = args.clone();
        v.push("-f=get_many");
        let r = latte.run(&db, &workload, "1000", &v);
        assert_latte_success(&r);
        assert_no_errors(&r);
    }
}
```

### 5.3 What the test proves

| phase | proves |
|---|---|
| populate | a full batched populate completes with zero errors |
| `get_many` | every partition holds exactly `rows_num` rows — no drops, no duplicates, no misplacement |
| `get_by_ck` | each global row index maps to the same `(pk, ck)` the row-granular API expects — the dataset is interchangeable with a row-written one |
| `count` | server-side count agrees, independent of paging |
| `-d 5s` | a time-based `--duration` is accepted and produces no errors (§1.3) |
| `-d 3 x total_batches` | running past `total_batches` wraps safely and leaves the data still valid |

Both cases must be exercised: the evenly divisible preset and the skewed one
with a ragged remainder, since only the latter produces short final batches and
a synthesised leftover partition group.

Keep `row_count` at 1000 so the whole test stays within the CI cluster's budget.

## 6. Edge cases

* `batch_size == 0` → error from `set_partition_batch_size`.
* `batch_size > n_rows_per_partition` → batch is the whole partition; `take`
  clamps to the partition size.
* Partition size not divisible by `batch_size` → last batch of that partition is
  short. This is expected; do not pad and do not reject the configuration.
* The preset's synthesised leftover group (single partition of arbitrary size,
  created by the row-count adjustment logic) must be handled like any other
  group — include a preset that produces one in the tests.
* `get_partition_batch` called before `set_partition_batch_size` → error naming
  the preset.
* Groups with no type-2 cycles (`c2 == 0`) → guard the `l2`/`r2` divisions.

---

## 7. Operational notes

* To populate exactly once, pass the `total_batches` value printed by
  `set_partition_batch_size` as `--duration`. Obtain it by running the workload
  once and reading the info line, then keep it in the test config. Passing
  `row_count` instead over-populates by roughly `row_count / total_batches`
  passes — harmless, just wasteful. Time-based durations are fully supported;
  see §1.3.
* `--rate` counts batches per second, not rows per second.
* Batch size should be kept small enough to stay under Scylla's
  `batch_size_warn_threshold_in_kb` / `batch_size_fail_threshold_in_kb`.

## 8. Performance targets

* `get_partition_batch`: `O(n_groups)` plus `O(batch_size)` divmods. Reference
  measurement on a 10M-row, 3-group preset: ~79 ns per call at `batch_size=4`
  and ~740 ns at `batch_size=100` — i.e. 7–20 ns per row, against ~83 ns per row
  for the existing per-row `get_partition_info` path.
* Preset memory must remain independent of partition count (~1 KB for a
  9-group, 28.9M-partition preset).

---

## 9. Implementation notes

Deviations from the plan above, found while implementing it. The plan sections
are left as originally written; this section records what actually changed.

### 9.1 A pre-existing bug had to be fixed first

`extract_validation_args` in `src/scripting/functions_common.rs` read its
integer arguments with `Value::as_signed()`, which rejects unsigned rune
integers. `partition.rows_num` is a `u64`, so every validation of the form
`[partition.rows_num, partition.rows_num, custom_err]` failed with
`Invalid validation arguments`.

This is a regression from commit `bc7e672` "Upgrade rune from 0.13 to 0.14".
Rune 0.13 matched on a single `Value::Integer` variant, so signedness did not
exist; 0.14 splits integers into signed and unsigned inline values and the
rewrite reached only for the signed one.

It went unnoticed because no workload that hits the broken path is covered by
the integration test. `workloads/row_count_validation.rn` is the only workload
passing `partition.rows_num` into `execute_prepared_with_validation` (its
`get_many` and `count` functions), and it is not referenced by any test. Its
`get` function passes a literal `1`, which is signed and therefore still works,
and `workloads/data_validation.rn` — which the test does cover — validates in
rune instead of using `execute_prepared_with_validation` at all.

Fixed by reading the arguments with `Value::as_integer::<u64>()`, which accepts
both signed and unsigned values.

Worth splitting into its own commit or PR, since it repairs an existing
workload. `workloads/row_count_validation.rn` is now covered by a new
`test_latte_cql_row_count_validation_workload` integration test, which exercises
`get`, `get_many` and `count` with both prepared and non-prepared statements, so
this class of regression cannot recur silently.

### 9.2 Test isolation is done by recreating the keyspace

The workload keeps an `erase` function because that is part of the workload
contract, but `erase` is by design invoked only implicitly by the `latte load`
command and is never called directly.

Since only `latte schema` and `latte run` are used here, the integration test
isolates its two cases by recreating the keyspace instead:
`latte schema ... -P recreate_keyspace=true`, following the convention of
`workloads/data_validation.rn`. The cases use different partition size presets,
so leftover rows would otherwise break the row count validation.

### 9.3 String parameters need embedded quotes

`-P` values are compiled as rune expressions, not strings
(`functions_common::param` parses the value with `ctx.parse_source::<ast::Expr>`).
An unquoted `-P partition_sizes=100:1` is therefore a rune syntax error reported
against line 1 of the script. String values must carry their own quotes:

```
-P partition_sizes="\"100:1\""          # shell
"-P", "partition_sizes=\"100:1\""       # rust integration test
```

### 9.4 `PartitionBatch.rows` uses `rune::alloc::Vec`

`#[rune(get)]` requires the field type to implement `TryClone`, which
`std::vec::Vec` does not. The field is a `rune::alloc::Vec<u64>` built from the
`Vec<u64>` that `RowDistributionPreset::get_partition_batch` returns, so the
internals and their unit tests stay on plain `std` types.

### 9.5 Integration tests share one DB cluster

The integration tests may run in parallel against a single cluster (they do
whenever `SCYLLA_TEST_HOST` is set, as in CI). `workloads/data_validation.rn`,
`workloads/integration_tests/binary_file.rn` and
`workloads/row_count_validation.rn` all default to the `latte` keyspace, so a
test that passes `recreate_keyspace=true` would drop tables out from under the
others.

The batch test therefore runs against its own keyspace,
`-P keyspace="latte_batch_partition_validation"`, and only recreates that one.
Note the embedded quotes - §9.3 applies to the keyspace name too, and without
them the const does not resolve and the script fails to compile with
`Missing item KEYSPACE`.

### 9.6 Files touched

| file | change |
|---|---|
| `src/scripting/row_distribution.rs` | preset fields, `GroupCycles`, the inverse mapping, `get_partition_batch`, the two rune functions, `PartitionBatch`, 7 unit tests |
| `src/scripting/mod.rs` | registration of the two functions and the new type |
| `src/scripting/functions_common.rs` | the §9.1 fix |
| `workloads/batch_partition_validation.rn` | new workload |
| `tests/integration_test.rs` | `parse_total_batches` helper, the batch workload test, and a `row_count_validation.rn` test covering the §9.1 gap |
