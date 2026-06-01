# Latte for DynamoDB / Alternator

**Benchmarks DynamoDB-compatible APIs (Amazon DynamoDB, ScyllaDB Alternator) using the same engine as CQL-based Latte**

Latte ships a dedicated `latte-alternator` binary that speaks the DynamoDB protocol instead of CQL.
It uses the dedicated ScyllaDB alternator-driver under the hood and supports all the same execution
features (async engine, rate/concurrency limiting, HDR histograms, reports, comparisons, etc.), plus
extra driver tuning options (datacenter/rack-aware routing, request compression, header optimization,
node-refresh intervals and partition-key route affinity — see [Driver tuning](#driver-tuning)).

## Installation

### From source

1. [Install Rust toolchain](https://rustup.rs/)
2. Build the alternator binary:

```shell
RUSTFLAGS="--cfg fetch_extended_version_info --cfg tokio_unstable" cargo install --path . --no-default-features --features alternator
```

### From release binaries

1. [Open Latte releases page on GitHub](https://github.com/scylladb/latte/releases)
2. Download `latte-alternator-<version>--<os>` for your platform

### From docker image

The `scylladb/latte` docker image contains both `latte` and `latte-alternator`.
Override the entrypoint to use the alternator binary:

```shell
docker run --entrypoint latte-alternator scylladb/latte:latest <args>
```

## Usage

Point `latte-alternator` at a DynamoDB-compatible HTTP endpoint:

```shell
latte-alternator schema <workload.rn> http://<host>:<port>
latte-alternator run <workload.rn> http://<host>:<port>
```

For ScyllaDB Alternator the default port is `8000`. For local DynamoDB the default is `8000` as well.

### AWS credentials

`latte-alternator` uses the standard AWS SDK credential chain. For local/Alternator usage
where authentication is not required, you can set dummy credentials:

```shell
export AWS_ACCESS_KEY_ID=dummy
export AWS_SECRET_ACCESS_KEY=dummy
export AWS_DEFAULT_REGION=us-east-1
```

### Driver tuning

`latte-alternator` accepts a set of additional options that tune the underlying ScyllaDB
alternator-driver. Unless noted otherwise, omitting an option leaves the driver default in place.

#### Routing and load balancing

`--datacenter` and `--rack` make the driver prefer nodes in the given datacenter and rack, narrowing
the routing scope (rack → datacenter → whole cluster). `--routing-fallback` controls whether the
driver falls back to a broader scope when the preferred one has no available nodes.

| Option | Value | Default | Description |
|--------|-------|---------|-------------|
| `--datacenter` | `DC` | — | Datacenter to route requests to |
| `--rack` | `RACK` | — | Rack to route requests to |
| `--routing-fallback` | `BOOL` | driver default | Fall back to broader scopes (rack → dc → cluster) if the preferred scope has no nodes |
| `--active-interval` | `MS` | driver default | Refresh interval for the known-nodes list while active |
| `--idle-interval` | `MS` | driver default | Refresh interval for the known-nodes list while idle |
| `--key-route-affinity` | `none` \| `rmw` \| `any-write` | `none` | Alternator write isolation mode for partition-key affinity routing |
| `--key-route-affinity-table` | `TABLE=PK` | — | Pre-configured partition-key attribute name for a table (repeatable, e.g. `users=user_id`) |

#### Request compression

| Option | Value | Default | Description |
|--------|-------|---------|-------------|
| `--request-compression` | `driver-default` \| `off` \| `gzip` \| `zlib` | `driver-default` | How to compress request bodies before signing |
| `--compression-threshold` | `BYTES` | `1024` | Minimum uncompressed body size before compression applies (`gzip` / `zlib` only) |
| `--compression-level` | `1`–`9` | driver default | Deflate compression level (`gzip` / `zlib` only) |

#### Headers

| Option | Value | Default | Description |
|--------|-------|---------|-------------|
| `--optimize-headers` | `BOOL` | driver default (`true`) | Strip request headers not used by Alternator before transmit |

Example invocations:

```shell
# Route to a specific datacenter and rack, with gzip request compression
latte-alternator run workloads/alternator/api_demo.rn http://<host>:8000 \
    --datacenter dc1 --rack rack1 --request-compression gzip --compression-level 6

# Enable read-modify-write partition-key affinity for a table
latte-alternator run workloads/alternator/api_demo.rn http://<host>:8000 \
    --key-route-affinity rmw --key-route-affinity-table users=user_id
```

## Workloads

Alternator workloads use the same Rune scripting language as CQL workloads.
The difference is the context API — instead of CQL-oriented `execute`/`execute_prepared`,
you use DynamoDB-style operations: `put`, `get`, `update`, `delete`, `query`, `scan`,
`batch_write_item`, `batch_get_item`, `create_table`, and `delete_table`.

Example workload scripts are in the [`workloads/alternator/`](workloads/alternator/) directory.

### Basic CRUD operations

```rust
use latte::*;

const TABLE = "my_table";

pub async fn schema(db) {
    db.delete_table(TABLE).await;
    db.create_table(TABLE, #{
        primary_key: "pk",
        sort_key: #{name: "sk", type: "N"}
    }).await?;
}

pub async fn run(db, i) {
    let pk = "user_" + i.to_string();
    let sk = 1;

    // PUT
    db.put(TABLE, #{
        pk: pk,
        sk: sk,
        name: "Random Name",
        age: 20,
        tags: ["a", "b", "c"]
    }, ()).await?;

    // GET
    let key = #{ pk: pk, sk: sk };
    db.get(TABLE, key, None).await?;

    // GET with consistent read
    db.get(TABLE, key, #{ consistent_read: true }).await?;

    // UPDATE
    db.update(TABLE, key, #{
        update: "SET #n = :new_name",
        attribute_names: #{ "#n": "name" },
        attribute_values: #{ ":new_name": "Updated Name" }
    }).await?;

    // QUERY
    db.query(TABLE, #{
        query: "pk = :pk",
        attribute_values: #{ ":pk": pk },
        limit: 10
    }).await?;

    // SCAN with filter
    db.scan(TABLE, #{
        filter: "age > :min",
        attribute_values: #{ ":min": 18 }
    }).await?;

    // DELETE
    db.delete(TABLE, key, ()).await?;
}
```

### Batch operations

```rust
use latte::*;

const TABLE = "batch_table";

pub async fn schema(db) {
    db.delete_table(TABLE).await;
    db.create_table(TABLE, "pk").await?;
}

pub async fn run(db, i) {
    let batch_size = 5;
    let base_id = `user_${i}_`;

    // Batch write
    let write_requests = #{};
    write_requests[TABLE] = [];
    for j in 0..batch_size {
        write_requests[TABLE].push(#{
            type: "put",
            item: #{ pk: `${base_id}${j}`, data: `item_${j}` }
        });
    }
    db.batch_write_item(write_requests, ()).await?;

    // Batch get
    let get_requests = #{};
    get_requests[TABLE] = [];
    for j in 0..batch_size {
        get_requests[TABLE].push(#{ pk: `${base_id}${j}` });
    }
    let results = db.batch_get_item(get_requests, #{
        consistent_read: true,
        with_result: true
    }).await?;
    assert!(results.len() == batch_size);

    // Batch delete
    let delete_requests = #{};
    delete_requests[TABLE] = [];
    for j in 0..batch_size {
        delete_requests[TABLE].push(#{
            type: "delete",
            key: #{ pk: `${base_id}${j}` }
        });
    }
    db.batch_write_item(delete_requests, ()).await?;
}
```

### Handling unprocessed items in batch writes

For large batch writes that may exceed provisioned throughput, you can manually
handle unprocessed items:

```rust
loop {
    let res = db.batch_write_item(write_requests, #{ get_unprocessed: true }).await?;
    if let Some(unprocessed) = res.get("unprocessed_items") {
        write_requests = unprocessed;
    } else {
        break;
    }
}
```

### Large objects and result retrieval

Use `with_result: true` to retrieve item data from GET operations:

```rust
use latte::*;

const TABLE = "large_objects_table";
const ROW_COUNT = latte::param!("rows", 1000);
const OBJECT_SIZE = latte::param!("size", 10240);
const WITH_RESULT = latte::param!("with_result", false);

pub async fn schema(db) {
    db.delete_table(TABLE).await;
    db.create_table(TABLE, "id").await?;
}

pub async fn insert(db, i) {
    db.put(TABLE, #{
        id: (i % ROW_COUNT).to_string(),
        data: latte::text(i, OBJECT_SIZE)
    }, ()).await?;
}

pub async fn run(db, i) {
    let id = (latte::hash(i) % ROW_COUNT).to_string();
    if WITH_RESULT {
        db.get(TABLE, #{ id: id }, #{ with_result: true }).await?;
    } else {
        db.get(TABLE, #{ id: id }, ()).await?;
    }
}
```

Run with:
```shell
latte-alternator schema workloads/alternator/large_objects.rn http://172.17.0.2:8000
latte-alternator run workloads/alternator/large_objects.rn -f insert -d 1000 http://172.17.0.2:8000
latte-alternator run workloads/alternator/large_objects.rn http://172.17.0.2:8000
latte-alternator run workloads/alternator/large_objects.rn http://172.17.0.2:8000 -P with_result=true
```

### Row count validation

Query results can be validated for expected row counts, same as in CQL workloads:

```rust
// Strict validation: exactly 1 row expected
db.query(TABLE, #{
    query: "pk = :pk",
    attribute_values: #{ ":pk": pk },
    validation: [1, "custom error message"]
}).await?;

// Range validation: between min and max rows
db.query(TABLE, #{
    query: "pk = :pk",
    attribute_values: #{ ":pk": pk },
    validation: [min_rows, max_rows]
}).await?;
```

### Data validation

Write data with known values, read it back with `with_result: true`, and assert correctness.
This pattern is useful for verifying data integrity under load — e.g. detecting corruption,
TTL-related deletions, or replication issues.

```rust
use latte::*;

const TABLE = "data_validation_table";
const ROW_COUNT = latte::param!("row_count", 1000);
const BLOB_SIZE = latte::param!("blob_size", 128);

pub async fn schema(db) {
    db.delete_table(TABLE).await;
    db.create_table(TABLE, "pk").await?;
}

// Helper: generate expected row data deterministically from cycle number
async fn generate_row(i) {
    let idx = i % ROW_COUNT;
    let pk = "item_" + idx.to_string();
    let name = latte::text(idx, 16);
    let score = latte::hash(idx) % 1000;
    let tags = ["tag_" + (idx % 5).to_string(), "tag_" + (idx % 3).to_string()];
    let payload = latte::blob(idx, BLOB_SIZE);
    #{
        pk: pk,
        name: name,
        score: score,
        tags: tags,
        payload: payload,
    }
}

pub async fn write(db, i) {
    let row = generate_row(i).await;
    db.put(TABLE, row, ()).await?;
}

pub async fn read(db, i) {
    let expected = generate_row(i).await;
    let result = db.get(TABLE, #{ pk: expected.pk }, #{
        consistent_read: true,
        with_result: true
    }).await?.unwrap();

    // Validate each field
    if result["name"] != expected.name {
        db.signal_failure(
            `Field 'name': expected '${expected.name}', got '${result["name"]}'`
        ).await?;
    }
    if result["score"] != expected.score {
        db.signal_failure(
            `Field 'score': expected '${expected.score}', got '${result["score"]}'`
        ).await?;
    }
    if result["tags"] != expected.tags {
        db.signal_failure(
            `Field 'tags': expected '${expected.tags}', got '${result["tags"]}'`
        ).await?;
    }
    if result["payload"] != expected.payload {
        db.signal_failure(
            `Field 'payload' mismatch for pk='${expected.pk}'`
        ).await?;
    }
}
```

Run with:
```shell
latte-alternator schema workloads/alternator/data_validation.rn http://172.17.0.2:8000
latte-alternator run -f write -d 1000 workloads/alternator/data_validation.rn http://172.17.0.2:8000
latte-alternator run -f read -d 60s workloads/alternator/data_validation.rn http://172.17.0.2:8000
```

Use `--validation-strategy` to control behavior on failure:
- `fail-fast` (default) — stop immediately on first mismatch
- `retry` — retry the read (useful for eventually-consistent scenarios)
- `ignore` — count failures but continue the benchmark

### Projection expressions

To retrieve only a subset of attributes rather than the entire item, you can use projection expressions in `get`, `query`, and `scan` operations. For `batch_get_item`, you can specify projection expressions on a per-table basis using an extended configuration object.

If the projected attributes contain DynamoDB reserved words (such as `name`, `status`, or `data`), you can use expression attribute names to define placeholders (starting with `#`) and map them to their actual names in the `attribute_names` parameter.

#### In GET, QUERY, and SCAN operations

```rust
// GET with projection expression and attribute names
db.get(TABLE, key, #{
    projection_expression: "#n, age",
    attribute_names: #{ "#n": "name" }
}).await?;

// QUERY with projection expression
db.query(TABLE, #{
    query: "pk = :pk",
    projection_expression: "#n, age",
    attribute_names: #{ "#n": "name" },
    attribute_values: #{ ":pk": pk }
}).await?;

// SCAN with projection expression
db.scan(TABLE, #{
    projection_expression: "pk, #d",
    attribute_names: #{ "#d": "data" }
}).await?;
```

#### In BATCH GET operations

For `batch_get_item`, instead of a list of keys, each table in the requests map can be configured with an object containing `keys`, and optionally `projection_expression` and `attribute_names`:

```rust
let requests = #{};
requests[TABLE] = #{
    keys: [
        #{ pk: "user_0" },
        #{ pk: "user_1" }
    ],
    projection_expression: "pk, #d",
    attribute_names: #{ "#d": "data" }
};

let result = db.batch_get_item(requests, #{
    with_result: true
}).await?;
```

The alternator driver supports all DynamoDB attribute value types:

| Rune type | DynamoDB type |
|-----------|---------------|
| `true` / `false` | Bool |
| integer (`42`) | N (Number) |
| float (`3.14`) | N (Number) |
| string (`"hello"`) | S (String) |
| bytes (`b"data"`) | B (Binary) |
| `string_set(["a", "b"])` | SS (String Set) |
| `number_set([1, 2, 3])` | NS (Number Set) |
| `binary_set([b"a", b"b"])` | BS (Binary Set) |
| vector (`[1, "two", true]`) | L (List) |
| object (`#{ key: "val" }`) | M (Map) |
| `Some(value)` | (inner value) |
| `None` | NULL |

### Conditional expressions

DynamoDB operations `put`, `delete`, and `update` support writing, deleting, or updating items conditionally based on a `condition_expression`.
When the condition is not met, the operation will fail with a `ConditionalCheckFailedException` (which is caught and handled by Latte's execution engine).

#### Put and delete operations with conditions

For `put` and `delete`, the condition expression and its expression parameter/value mappings are specified in the optional third argument `options` object:

```rust
// PUT only if the item does not already exist
db.put(TABLE, #{
    pk: pk,
    sk: sk,
    name: "New User",
    age: 25
}, #{
    condition_expression: "attribute_not_exists(pk)"
}).await?;

// DELETE only if the user is older than 18
db.delete(TABLE, #{ pk: pk, sk: sk }, #{
    condition_expression: "age > :min_age",
    attribute_values: #{ ":min_age": 18 }
}).await?;
```

If no options are needed for `put` or `delete`, pass `()` (unit) as the third argument:
```rust
db.put(TABLE, item, ()).await?;
db.delete(TABLE, key, ()).await?;
```

#### Update operations with conditions

For `update`, the condition expression and its parameter/value mappings are included directly inside the `params` (third) argument alongside the `update` expression itself:

```rust
// UPDATE only if the current age matches the expected age
db.update(TABLE, #{ pk: pk, sk: sk }, #{
    update: "SET #n = :new_name",
    condition_expression: "age = :expected_age",
    attribute_names: #{ "#n": "name" },
    attribute_values: #{
        ":new_name": "Updated Name",
        ":expected_age": 21
    }
}).await?;
```

### Table creation options

The `create_table` function supports two forms:

```rust
// Simple: partition key only (defaults to String type)
db.create_table("my_table", "pk").await?;

// Full: partition key + sort key with explicit types
db.create_table("my_table", #{
    primary_key: "pk",
    sort_key: #{ name: "sk", type: "N" }  // "S", "N", or "B"
}).await?;
```

## Context API Reference

| Method | Description |
|--------|-------------|
| `db.create_table(name, schema)` | Create a DynamoDB table |
| `db.delete_table(name)` | Delete a table (ignores errors if not found) |
| `db.put(table, item, options)` | PutItem |
| `db.get(table, key, options)` | GetItem |
| `db.update(table, key, options)` | UpdateItem |
| `db.delete(table, key, options)` | DeleteItem |
| `db.query(table, options)` | Query |
| `db.scan(table, options)` | Scan |
| `db.batch_write_item(requests, options)` | BatchWriteItem |
| `db.batch_get_item(requests, options)` | BatchGetItem |
| `db.elapsed_secs()` | Seconds since workload start |

## Example workloads

Ready-to-use example workloads are available in [`workloads/alternator/`](workloads/alternator/):

| File | Description |
|------|-------------|
| [`api_demo.rn`](workloads/alternator/api_demo.rn) | Full CRUD demo covering all operations |
| [`batch_operations.rn`](workloads/alternator/batch_operations.rn) | Batch write/get/delete with assertions |
| [`manual_batch_operations.rn`](workloads/alternator/manual_batch_operations.rn) | Handling unprocessed items in batch writes |
| [`large_objects.rn`](workloads/alternator/large_objects.rn) | Large object insertion and retrieval benchmarks |
| [`row_count_validation.rn`](workloads/alternator/row_count_validation.rn) | Query result row count validation with partition presets |
| [`type_validation.rn`](workloads/alternator/type_validation.rn) | All supported DynamoDB data types with round-trip assertions |
