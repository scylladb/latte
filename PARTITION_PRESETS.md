# Partition row distribution presets

Real-life data sets rarely have uniform partitions. A table usually holds a lot of
small partitions, fewer medium ones and a handful of very large ones, and the
behaviour of the database depends a lot on that shape.

A *partition row distribution preset* lets a workload describe such a shape
declaratively and then address it from the stress functions. Latte turns the
stress iteration index into a partition index for you, keeping the requested
proportions and spreading consecutive iterations over different partitions.

The feature consists of three parts:

| | |
|---|---|
| [Defining a preset](#defining-a-preset) | describe the wanted partition sizes and their proportions |
| [Addressing rows](#addressing-rows) | turn a stress iteration index into a partition index and its rows number |
| [Single-partition batches](#single-partition-batches) | write whole batches that never span more than one partition |

Presets are available both in the CQL and the Alternator flavours of latte. The
batching examples below use the CQL `db.batch_prepared` function, the Alternator
flavour has its own batch operations - see [ALTERNATOR.md](ALTERNATOR.md).

---

## Quick start

```rust
const ROW_COUNT = latte::param!("row_count", 1000000);
const ROWS_PER_PARTITION = latte::param!("rows_per_partition", 1);
const PARTITION_SIZES = latte::param!("partition_sizes", "70:1,20:2.5,10:3.5");

pub async fn prepare(db) {
    db.init_partition_row_distribution_preset(
        "main", ROW_COUNT, ROWS_PER_PARTITION, PARTITION_SIZES).await?;
    db.prepare("insert", `INSERT INTO ks.t(pk, ck) VALUES (:pk, :ck)`).await?;
}

pub async fn insert(db, i) {
    let idx = i % ROW_COUNT;
    let partition = db.get_partition_info("main", idx).await;
    db.execute_prepared("insert", [hash(partition.idx), hash(idx)]).await?
}
```

---

## Defining a preset

```rust
db.init_partition_row_distribution_preset(
    preset_name, row_count, rows_per_partitions_base, rows_per_partitions_groups).await?;
```

| parameter | meaning |
|---|---|
| `preset_name` | name to address the preset by later. Must not be empty |
| `row_count` | total number of rows the preset must cover |
| `rows_per_partitions_base` | base partition size, multiplied by the group multipliers below |
| `rows_per_partitions_groups` | `percent:multiplier` pairs separated by commas |

`rows_per_partitions_groups` is the shape of the data set.
For `"70:1,20:2.5,10:3.5"` with `rows_per_partitions_base=10`:

- `70%` of the partitions hold `10` rows (`10 * 1`)
- `20%` of the partitions hold `25` rows (`10 * 2.5`)
- `10%` of the partitions hold `35` rows (`10 * 3.5`)

Rules:

- the percentages must sum up to `100`
- duplicate `percent:multiplier` pairs are rejected
- both parts may be fractional, e.g. `"49.1:1,49:2,1.9:2.5"`
- an empty value means `"100:1"`, i.e. uniform partitions

Latte derives the number of partitions from `row_count` and the requested
proportions and prints what it came up with:

```
info: init_partition_row_distribution_preset: preset_name=main, total_partitions=2000, \
total_rows=10000, partitions/rows -> 1000(~50%):6, 1000(~50%):4
```

Because the requested proportions rarely divide `row_count` evenly, the preset
adjusts the partition counts to land on exactly `row_count` rows, and may add one
extra partition of a leftover size. That partition shows up as an additional
group in the printed line, and the workload does not need to care about it.

### Parameters must match between runs

A preset is not stored in the database, it is recomputed on every latte
invocation. So a populating run and a later reading run must be given the **same**
`row_count`, `rows_per_partitions_base` and `rows_per_partitions_groups`, or the
reading run will expect a different partition layout than the one that was
written.

### Quoting the parameters

`-P` values are compiled as Rune expressions, so a string value must carry its
own quotes:

```bash
-P partition_sizes="\"70:1,20:2.5,10:3.5\""    # correct
-P partition_sizes=70:1,20:2.5,10:3.5          # Rune syntax error
```

The same applies to any other string parameter, for example `-P keyspace="\"ks\""`.

---

## Addressing rows

Two functions turn a stress iteration index into a partition:

```rust
// Partition index only
let partition_idx = db.get_partition_idx("main", idx).await;

// Partition index together with the number of rows that partition holds
let partition = db.get_partition_info("main", idx).await;
partition.idx       // partition index, may be shifted: 'partition.idx += OFFSET'
partition.rows_num  // number of rows of that partition
```

The index is taken modulo the total number of rows, so any value is valid.
Consecutive indexes are deliberately mapped to *different* partitions, which
keeps reads spread over the cluster and lets the skewed groups be weighted
fairly against each other.

Turn the partition index into a partition key the usual way, most workloads use
`hash()`:

```rust
let pk = hash(partition.idx);
let ck = hash(idx);
```

### Validating the number of rows

`partition.rows_num` is the exact number of rows a partition must hold, which
makes it a ready-made expectation for the row count validation functions.

An exact expectation is a **single** value:

```rust
db.execute_prepared_with_validation(
    "get_many",
    [pk, partition.rows_num + 10],
    [partition.rows_num], // exactly this number of rows
).await?
```

**Two** values mean an inclusive range, so use that form only when the expected
number of rows may vary:

```rust
let rows_min = if elapsed > 100.0 { 0 } else { partition.rows_num };
db.execute_prepared_with_validation(
    "get_many",
    [pk, partition.rows_num + 10],
    [rows_min, partition.rows_num], // anywhere between the two, inclusive
).await?
```

A custom error message may be appended to either form:

| validation argument | meaning |
|---|---|
| `[n]` | exactly `n` rows |
| `[n, m]` | between `n` and `m` rows, both inclusive |
| `[n, custom_err]` | exactly `n` rows, with a custom error message |
| `[n, m, custom_err]` | between `n` and `m` rows, with a custom error message |

See the "Validating number of rows for SELECT queries" section of the
[README](README.md#validating-number-of-rows-for-select-queries) for more
details.

---

## Single-partition batches

### Why

Latte sends `LOGGED` batches. ScyllaDB and Cassandra skip the batchlog when every
statement of a `LOGGED` batch targets the **same** partition, because such a batch
is already atomic. A batch spanning several partitions makes the coordinator
durably write the batch to two batchlog replicas before anything is applied.

Since a preset deliberately maps consecutive stress iterations to *different*
partitions, building a batch out of consecutive iteration indexes would produce a
multi-partition batch and pay that price on every write. The functions below give
a batch whose rows all belong to one partition, while keeping the partition size
proportions the preset provides.

### Enabling it

Call `set_partition_batch_size` in `prepare`, right after the preset is created:

```rust
pub async fn prepare(db) {
    db.init_partition_row_distribution_preset(
        "main", ROW_COUNT, ROWS_PER_PARTITION, PARTITION_SIZES).await?;
    db.set_partition_batch_size("main", BATCH_SIZE).await?;
    ...
}
```

It prints the number of batches needed to write every row exactly once:

```
info: set_partition_batch_size: preset_name=main, batch_size=2, total_batches=5000
```

`batch_size` is an **upper bound**, not an exact size. A batch never spans two
partitions, so the last batch of a partition is shorter when the partition size
is not a multiple of `batch_size`. A `batch_size` bigger than a partition simply
yields that whole partition as one batch.

### Writing batches

```rust
pub async fn insert_batch(db, i) {
    let batch = db.get_partition_batch("main", i).await?;
    let pk = hash(batch.idx);
    let stmt_names = [];
    let stmt_params = [];
    for idx in batch.rows {
        stmt_names.push("insert");
        stmt_params.push([pk, hash(idx)]);
    }
    db.batch_prepared(stmt_names, stmt_params).await?
}
```

`get_partition_batch` takes the stress iteration index **as is** and returns:

| field | meaning |
|---|---|
| `batch.idx` | index of the partition all the rows of the batch belong to |
| `batch.rows` | stress iteration indexes of the rows of the batch, never empty |

The stress iteration index enumerates *batches*, not rows. Consecutive iterations
walk the partitions first and only then advance to the next batch of each
partition, so consecutive batches keep landing on different partitions:

```
 i | partition | rows of the partition written
---+-----------+------------------------------
 0 |    P0     | rows 0-1
 1 |    P1     | rows 0-1
 2 |    P2     | rows 0-1
...
 N |    P0     | rows 2-3      <- all the partitions were visited, so the next
 N |           |                  batch of every partition follows
```

The values in `batch.rows` are the very same stress iteration indexes that
`get_partition_info` maps to that partition. A batch-written data set is
therefore identical to a row-by-row written one, and the reading functions of a
workload keep using `get_partition_info` with no knowledge of the batch size.

### Choosing the run duration

`total_batches` is the number of iterations needed to write every row exactly
once. Take it from the printed line and pass it as `--duration` of the populating
command. Any other duration stays valid, because the stress iteration index wraps
around:

| intent | `--duration` | outcome |
|---|---|---|
| populate exactly once | `total_batches` | full coverage, nothing rewritten |
| time boxed throughput run | e.g. `30m` | correct data, partial or repeated coverage |
| longer than needed | `> total_batches` | rows get rewritten, data stays correct |
| shorter than needed | `< total_batches` | only a part of the partitions gets written |

A too small value is the most common mistake. It does not report an error, it
just leaves a part of the partitions empty, which the reading validation
functions then report as missing rows.

Note that `--rate` and the reported throughput count **batches**, not rows, once
the workload writes batches.

### Batch size and the server-side limits

Keep `batch_size` small enough to stay below the ScyllaDB
`batch_size_warn_threshold_in_kb` and `batch_size_fail_threshold_in_kb` limits.
A few dozen rows per batch is a usual choice. Batching whole multi-thousand-row
partitions in a single statement is not.

---

## Ready to use workload

Everything described above is already put together in
**[`workloads/batch_partition_validation.rn`](workloads/batch_partition_validation.rn)**.
Take it as is, or copy it as the starting point of your own workload - the
tricky parts are already solved there.

| Function | Kind | What it does |
|---|---|---|
| `schema` | latte-reserved | creates the keyspace and the table, optionally recreating the keyspace |
| `prepare` | latte-reserved | creates the preset, enables batching and prepares the statements |
| `insert_batch` | user | writes one single-partition `LOGGED` batch per stress iteration |
| `get_many` | user | checks that every partition holds exactly the number of rows the preset declares |
| `get_by_ck` | user | checks that every single row sits under the `pk`/`ck` pair the row-granular API expects |
| `count` | user | checks the same rows number as `get_many`, but with a server side `SELECT COUNT(...)` |

The **latte-reserved** functions are recognized by their names and are never
selected with `-f`. They take a single `db` argument, and latte calls them on its
own: `schema` runs on the `latte schema` command and `prepare` runs
automatically before the workload of every `latte run`.

The **user** functions are the workload itself. They take `(db, i)`, where `i` is
the stress iteration index, and one of them is picked per run with `-f`.

The script also carries a `USAGE` comment block with the same commands as below,
so they stay next to the code they run.

### Running it

Create the schema:

```bash
latte schema workloads/batch_partition_validation.rn 172.17.0.2 -P tablets=false
```

Populate. Here `50%` of the partitions hold 4 rows and `50%` hold 6, written in
batches of 2 rows. `1000` partitions of 6 rows need 3 batches each and `1000`
partitions of 4 rows need 2 batches each, so `--duration` is `1000*3 + 1000*2 = 5000`,
which is exactly the `total_batches` value the run prints:

```bash
latte run workloads/batch_partition_validation.rn -q \
    -d 5000 -P row_count=10000 \
    -P rows_per_partition=1 -P partition_sizes="\"50:4,50:6\"" \
    -P batch_size=2 -f insert_batch -- 172.17.0.2
```

Validate that every partition holds the expected number of rows, that every
single row sits where the row-granular API expects it, and that a server-side
count agrees:

```bash
latte run workloads/batch_partition_validation.rn -q \
    -d 10000 -P row_count=10000 \
    -P rows_per_partition=1 -P partition_sizes="\"50:4,50:6\"" \
    -f get_many -- 172.17.0.2

latte run workloads/batch_partition_validation.rn -q \
    -d 10000 -P row_count=10000 \
    -P rows_per_partition=1 -P partition_sizes="\"50:4,50:6\"" \
    -f get_by_ck -- 172.17.0.2

latte run workloads/batch_partition_validation.rn -q \
    -d 10000 -P row_count=10000 \
    -P rows_per_partition=1 -P partition_sizes="\"50:4,50:6\"" \
    -f count -- 172.17.0.2
```

The reading commands do not need `batch_size`, it affects the write path only.
The other preset parameters must match the populating command.

---

## Multiple presets

The number of presets is not limited and they are addressed by name, so a single
workload may keep a separate preset per table:

```rust
pub async fn prepare(db) {
    db.init_partition_row_distribution_preset("users", USER_ROWS, 1, "100:1").await?;
    db.init_partition_row_distribution_preset("events", EVENT_ROWS, 10, "90:1,10:5").await?;
    db.set_partition_batch_size("events", 20).await?;
}
```

Batching is enabled per preset, so one table may be written in batches while
another is written row by row.

---

## Function reference

| Function | Description |
|---|---|
| `db.init_partition_row_distribution_preset(name, row_count, base, groups)` | creates a preset |
| `db.get_partition_idx(name, idx)` | partition index for a stress iteration index |
| `db.get_partition_info(name, idx)` | `{idx, rows_num}` for a stress iteration index |
| `db.set_partition_batch_size(name, batch_size)` | enables single-partition batches, prints `total_batches` |
| `db.get_partition_batch(name, idx)` | `{idx, rows}` of the batch a stress iteration index addresses |
