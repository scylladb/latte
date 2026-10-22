use crate::scripting::context::Context;
use crate::scripting::db_error::{DbError, DbErrorKind};
use crate::scripting::rune_uuid::Uuid;
use crate::scripting::Resources;
use chrono::Utc;
use metrohash::MetroHash64;
use rand::distr::Distribution;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Normal, Uniform};
use rune::macros::{quote, MacroContext, TokenStream};
use rune::parse::Parser;
use rune::runtime::{Function, Ref, VmError, VmResult};
use rune::{ast, vm_try, Value};
use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io;
use std::io::{BufRead, BufReader, ErrorKind, Read};

/// Returns the literal value stored in the `params` map under the key given as the first
/// macro arg, and if not found, returns the expression from the second arg.
pub fn param(
    ctx: &mut MacroContext,
    params: &HashMap<String, String>,
    ts: &TokenStream,
) -> rune::compile::Result<TokenStream> {
    let mut parser = Parser::from_token_stream(ts, ctx.macro_span());
    let name = parser.parse::<ast::LitStr>()?;
    let name = ctx.resolve(name)?.to_string();
    let _ = parser.parse::<ast::Comma>()?;
    let expr = parser.parse::<ast::Expr>()?;
    let rhs = match params.get(&name) {
        Some(value) => {
            let src_id = ctx.insert_source(&name, value)?;
            let value = ctx.parse_source::<ast::Expr>(src_id)?;
            quote!(#value)
        }
        None => quote!(#expr),
    };
    Ok(rhs.into_token_stream(ctx)?)
}

pub struct ValidationArgs {
    pub expected_min: u64,
    pub expected_max: u64,
    pub custom_err_msg: String,
}

/// Extracts validation arguments from the given vector of Values.
///
/// The expected formats are:
///  * [Integer] -> Exact number of expected rows
///  * [Integer, Integer] -> Range of expected rows, both values are inclusive.
///  * [Integer, String] -> Exact number of expected rows and custom error message.
///  * [Integer, Integer, String] -> Range of expected rows and custom error message.
pub fn extract_validation_args(validation_args: Vec<Value>) -> Result<ValidationArgs, String> {
    // NOTE: 'as_integer' accepts both signed and unsigned rune integers. Values
    //       coming from the partitions preset, such as 'partition.rows_num', are
    //       unsigned and would be rejected by a signed-only accessor.
    let as_int = |v: &Value| v.as_integer::<u64>().ok();
    let as_str = |v: &Value| {
        v.borrow_ref::<rune::alloc::String>()
            .ok()
            .map(|s| s.as_str().to_string())
    };
    match validation_args.as_slice() {
        // (int): expected_rows
        [a] if as_int(a).is_some() => {
            let n = as_int(a).unwrap();
            Ok(ValidationArgs {
                expected_min: n,
                expected_max: n,
                custom_err_msg: String::new(),
            })
        }
        // (int, int): expected_rows_num_min, expected_rows_num_max
        [a, b] if as_int(a).is_some() && as_int(b).is_some() => Ok(ValidationArgs {
            expected_min: as_int(a).unwrap(),
            expected_max: as_int(b).unwrap(),
            custom_err_msg: String::new(),
        }),
        // (int, str): expected_rows, custom_err_msg
        [a, b] if as_int(a).is_some() && as_str(b).is_some() => {
            let n = as_int(a).unwrap();
            Ok(ValidationArgs {
                expected_min: n,
                expected_max: n,
                custom_err_msg: as_str(b).unwrap(),
            })
        }
        // (int, int, str): expected_rows_num_min, expected_rows_num_max, custom_err_msg
        [a, b, c] if as_int(a).is_some() && as_int(b).is_some() && as_str(c).is_some() => {
            Ok(ValidationArgs {
                expected_min: as_int(a).unwrap(),
                expected_max: as_int(b).unwrap(),
                custom_err_msg: as_str(c).unwrap(),
            })
        }
        _ => Err("Invalid validation arguments".to_string()),
    }
}

/// Creates a new UUID for current iteration
#[rune::function]
pub fn uuid(i: i64) -> Uuid {
    Uuid::new(i)
}

/// Computes a hash of an integer value `i`.
/// Returns a value in range `0..i64::MAX`.
fn hash_inner(i: i64) -> i64 {
    let mut hash = MetroHash64::new();
    i.hash(&mut hash);
    (hash.finish() & 0x7FFFFFFFFFFFFFFF) as i64
}

/// Computes a hash of an integer value `i`.
/// Returns a value in range `0..i64::MAX`.
#[rune::function]
pub fn hash(i: i64) -> i64 {
    hash_inner(i)
}

/// Computes hash of two integer values.
#[rune::function]
pub fn hash2(a: i64, b: i64) -> i64 {
    hash2_inner(a, b)
}

fn hash2_inner(a: i64, b: i64) -> i64 {
    let mut hash = MetroHash64::new();
    a.hash(&mut hash);
    b.hash(&mut hash);
    (hash.finish() & 0x7FFFFFFFFFFFFFFF) as i64
}

/// Computes a hash of an integer value `i`.
/// Returns a value in range `0..max`.
#[rune::function]
pub fn hash_range(i: i64, max: i64) -> i64 {
    hash_inner(i) % max
}

/// Generates a 64-bits floating point value with normal distribution
#[rune::function]
pub fn normal(i: i64, mean: f64, std_dev: f64) -> VmResult<f64> {
    let mut rng = SmallRng::seed_from_u64(i as u64);
    let distribution =
        vm_try!(Normal::new(mean, std_dev).map_err(|e| VmError::panic(format!("{e}"))));
    VmResult::Ok(distribution.sample(&mut rng))
}

/// Generates a 32-bits floating point value with normal distribution
#[rune::function]
pub fn normal_f32(i: i64, mean: f32, std_dev: f32) -> VmResult<f32> {
    let mut rng = SmallRng::seed_from_u64(i as u64);
    let distribution: Normal<f64> = vm_try!(
        Normal::new(mean.into(), std_dev.into()).map_err(|e| VmError::panic(format!("{e}")))
    );
    VmResult::Ok(distribution.sample(&mut rng) as f32)
}

#[rune::function]
pub fn uniform(i: i64, min: f64, max: f64) -> VmResult<f64> {
    let mut rng = SmallRng::seed_from_u64(i as u64);
    let distribution = vm_try!(Uniform::new(min, max).map_err(|e| VmError::panic(format!("{e}"))));
    VmResult::Ok(distribution.sample(&mut rng))
}

/// Generates random blob of data of given length.
/// Parameter `seed` is used to seed the RNG.
#[rune::function]
pub fn blob(seed: i64, len: usize) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(seed as u64);
    (0..len).map(|_| rng.random::<u8>()).collect()
}

/// Shortest and longest segment of a compressible blob. 8 bytes is above the 4 bytes minimum match
/// of LZ4 and zstd, so both compressors see every repeated segment.
const COMPRESSIBLE_BLOB_SEGMENT_LEN: std::ops::RangeInclusive<usize> = 8..=64;

/// How far back a repeated segment of a compressible blob may copy from. It is kept well below
/// the 4 KiB chunks ScyllaDB compresses by default, so most copies sit in the same chunk as their
/// source, which is the only place a compressor without a dictionary can find them.
const COMPRESSIBLE_BLOB_MAX_DISTANCE: usize = 1024;

/// Generates a compressible binary blob of given length.
/// Like 'blob', it is fully defined by its parameters, and 'seed' is used to seed the RNG.
///
/// The blob is a series of 8..=64 bytes segments:
/// - with 'repeat_pct'% probability a segment repeats bytes from at most 1 KiB back in the blob,
///   which LZ4, zstd and the other LZ77 based compressors shrink;
/// - otherwise it holds literal bytes, each one of 'alphabet_size' distinct values, which only the
///   entropy coding compressors, such as zstd, shrink: to 'log2(alphabet_size)' bits per byte at best.
///
/// So 'repeat_pct' sets the savings of every compressor, and a smaller 'alphabet_size' adds savings
/// for zstd only. 'repeat_pct = 0' with 'alphabet_size = 256' is as incompressible as 'blob'.
#[rune::function]
pub fn compressible_blob(
    seed: i64,
    len: usize,
    repeat_pct: i64,
    alphabet_size: i64,
) -> VmResult<Vec<u8>> {
    let (repeat_pct, alphabet_size) =
        vm_try!(check_compressible_blob_args(repeat_pct, alphabet_size));
    VmResult::Ok(compressible_blob_inner(
        seed,
        len,
        repeat_pct,
        alphabet_size,
    ))
}

fn check_compressible_blob_args(
    repeat_pct: i64,
    alphabet_size: i64,
) -> Result<(u32, u32), VmError> {
    if !(0..=100).contains(&repeat_pct) {
        return Err(VmError::panic(format!(
            "compressible_blob: 'repeat_pct' must be in the 0..=100 range, got {repeat_pct}"
        )));
    }
    if !(1..=256).contains(&alphabet_size) {
        return Err(VmError::panic(format!(
            "compressible_blob: 'alphabet_size' must be in the 1..=256 range, got {alphabet_size}"
        )));
    }
    Ok((repeat_pct as u32, alphabet_size as u32))
}

fn compressible_blob_inner(seed: i64, len: usize, repeat_pct: u32, alphabet_size: u32) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(seed as u64);
    let mut result = Vec::with_capacity(len);
    while result.len() < len {
        let segment_len = rng
            .random_range(COMPRESSIBLE_BLOB_SEGMENT_LEN)
            .min(len - result.len());
        if !result.is_empty() && rng.random_range(0..100) < repeat_pct {
            let max_distance = result.len().min(COMPRESSIBLE_BLOB_MAX_DISTANCE);
            let start = result.len() - rng.random_range(1..=max_distance);
            // NOTE: copy byte by byte, the source may overlap the bytes being written
            for k in 0..segment_len {
                result.push(result[start + k]);
            }
        } else {
            for _ in 0..segment_len {
                // Spread the values of the alphabet over the whole byte range
                result.push((rng.random_range(0..alphabet_size) * 256 / alphabet_size) as u8);
            }
        }
    }
    result
}

/// Generates random string of given length.
/// Parameter `seed` is used to seed
/// the RNG.
#[rune::function]
pub fn text(seed: i64, len: usize) -> String {
    let charset: Vec<char> = ("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".to_owned()
        + "0123456789!@#$%^&*()_+-=[]{}|;:',.<>?/")
        .chars()
        .collect();
    let mut rng = SmallRng::seed_from_u64(seed as u64);
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx]
        })
        .collect()
}

#[rune::function]
pub fn vector(len: usize, generator: Function) -> VmResult<Vec<Value>> {
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let value = vm_try!(generator.call((i,)));
        result.push(value);
    }
    VmResult::Ok(result)
}

/// Generates 'now' timestamp
#[rune::function]
pub fn now_timestamp() -> i64 {
    Utc::now().timestamp()
}

/// Selects one item from the collection based on the hash of the given value.
#[rune::function]
pub fn hash_select(i: i64, collection: &[Value]) -> Value {
    collection[(hash_inner(i) % collection.len() as i64) as usize].clone()
}

/// Joins all strings in vector with given separator
#[rune::function]
pub fn join(collection: &[Value], separator: &str) -> VmResult<String> {
    let mut result = String::new();
    let mut first = true;
    for v in collection {
        let v = vm_try!(v.clone().into_string());
        if !first {
            result.push_str(separator);
        }
        result.push_str(v.as_str());
        first = false;
    }
    VmResult::Ok(result)
}

/// Checks whether input value is of None type or not
#[rune::function]
pub fn is_none(input: Value) -> bool {
    // NOTE: The reason to add it is that following rune code doesn't work with 'None' type:
    //   let result = if row.some_col == None { "None" } else { row.some_col };
    // With this function it is possible to check for None the following way:
    //   let result = if is_none(row.some_col) { "None" } else { row.some_col };
    //   println!("DEBUG: value for some_col is '{result}'", result=result);
    if let Ok(opt) = input.borrow_ref::<Option<Value>>() {
        return opt.is_none();
    }
    false
}

/// Reads a file into a string.
#[rune::function]
pub fn read_to_string(filename: &str) -> io::Result<String> {
    let mut file = File::open(filename).expect("no such file");

    let mut buffer = String::new();
    file.read_to_string(&mut buffer)?;

    Ok(buffer)
}

/// Reads a file into a vector of lines.
#[rune::function]
pub fn read_lines(filename: &str) -> io::Result<Vec<String>> {
    let file = File::open(filename).expect("no such file");
    let buf = BufReader::new(file);
    let result = buf
        .lines()
        .map(|l| l.expect("Could not parse line"))
        .collect();
    Ok(result)
}

/// Reads a file into a vector of words.
#[rune::function]
pub fn read_words(filename: &str) -> io::Result<Vec<String>> {
    let file = File::open(filename)
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to open file {filename}: {e}")))?;
    let buf = BufReader::new(file);
    let mut result = Vec::new();
    for line in buf.lines() {
        let line = line?;
        let words = line
            .split(|c: char| !c.is_alphabetic())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        result.extend(words);
    }
    Ok(result)
}

/// Reads a resource file as a string.
fn read_resource_to_string_inner(path: &str) -> io::Result<String> {
    let resource = Resources::get(path).ok_or_else(|| {
        io::Error::new(ErrorKind::NotFound, format!("Resource not found: {path}"))
    })?;
    let contents = std::str::from_utf8(resource.data.as_ref())
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("Invalid UTF8 string: {e}")))?;
    Ok(contents.to_string())
}

#[rune::function]
pub fn read_resource_to_string(path: &str) -> io::Result<String> {
    read_resource_to_string_inner(path)
}

#[rune::function]
pub fn read_resource_lines(path: &str) -> io::Result<Vec<String>> {
    Ok(read_resource_to_string_inner(path)?
        .split('\n')
        .map(|s| s.to_string())
        .collect())
}

#[rune::function]
pub fn read_resource_words(path: &str) -> io::Result<Vec<String>> {
    Ok(read_resource_to_string_inner(path)?
        .split(|c: char| !c.is_alphabetic())
        .map(|s| s.to_string())
        .collect())
}

#[rune::function(instance)]
pub async fn signal_failure(_ctx: Ref<Context>, message: Ref<str>) -> Result<(), DbError> {
    let err = DbError::new(DbErrorKind::CustomError(message.to_string()));
    Err(err)
}

#[rune::function(instance)]
pub fn elapsed_secs(ctx: &Context) -> f64 {
    ctx.start_time.try_lock().unwrap().elapsed().as_secs_f64()
}

/// Rejects calls that write run-level report state from a worker-cloned
/// context: workers operate on per-thread copies that are never merged back,
/// so such writes would be silently lost.
fn reject_in_workload(ctx: &Context, function: &str) -> Result<(), VmError> {
    if ctx.is_worker_clone {
        return Err(VmError::panic(format!(
            "{function} is only allowed in single-threaded setup functions \
             such as prepare, schema or erase; called from a workload function \
             or prepare_worker it would have no effect. Use record_metric for per-cycle values."
        )));
    }
    Ok(())
}

/// Rejects per-cycle calls from a setup function (prepare/schema/erase): those
/// run on the original context whose stats are reset before the run, so the
/// value would be silently dropped. The mirror of `reject_in_workload`.
fn reject_in_setup(ctx: &Context, function: &str) -> Result<(), VmError> {
    if !ctx.is_worker_clone || ctx.in_prepare_worker {
        return Err(VmError::panic(format!(
            "{function} only takes effect inside workload functions; called from \
             a setup function such as prepare, prepare_worker, schema or erase its \
             value would be discarded before the run."
        )));
    }
    Ok(())
}

#[rune::function(instance)]
pub fn set_report_field(ctx: &Context, key: Ref<str>, value: Ref<str>) -> VmResult<()> {
    vm_try!(reject_in_workload(ctx, "set_report_field"));
    ctx.set_report_field(&key, &value);
    VmResult::Ok(())
}

#[rune::function(instance)]
pub fn record_metric(ctx: &Context, name: Ref<str>, value: f64) -> VmResult<()> {
    vm_try!(reject_in_setup(ctx, "record_metric"));
    if !value.is_finite() {
        return VmResult::panic(format!(
            "record_metric: value for metric \"{}\" must be a finite number, got {value}",
            &*name
        ));
    }
    ctx.record_metric(&name, value);
    VmResult::Ok(())
}

#[rune::function(instance)]
pub fn declare_metric(ctx: &Context, name: Ref<str>, orientation: Ref<str>) -> VmResult<()> {
    vm_try!(reject_in_workload(ctx, "declare_metric"));
    let orientation = vm_try!(match &*orientation {
        "higher" => Ok(1),
        "lower" => Ok(-1),
        other => Err(VmError::panic(format!(
            "declare_metric: orientation must be \"higher\" or \"lower\", got \"{other}\""
        ))),
    });
    ctx.declare_metric(&name, orientation);
    VmResult::Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::config::{RetryInterval, ValidationStrategy};

    /// Validation arguments must be accepted no matter whether the rune values
    /// are signed or unsigned. Values taken from the partitions preset, such as
    /// 'partition.rows_num', are unsigned.
    /// Ref: a regression of the 'Upgrade rune from 0.13 to 0.14' change, which
    /// left 'extract_validation_args' reading signed integers only.
    #[test]
    fn extract_validation_args_accepts_unsigned_integers() {
        let signed = |v: i64| rune::to_value(v).expect("failed to build a signed value");
        let unsigned = |v: u64| rune::to_value(v).expect("failed to build an unsigned value");
        let text = |v: &str| rune::to_value(v).expect("failed to build a string value");

        for (label, args) in [
            ("[uint]", vec![unsigned(7)]),
            ("[uint, uint]", vec![unsigned(7), unsigned(9)]),
            ("[uint, str]", vec![unsigned(7), text("boom")]),
            (
                "[uint, uint, str]",
                vec![unsigned(7), unsigned(9), text("boom")],
            ),
            (
                "[int, uint, str]",
                vec![signed(7), unsigned(9), text("boom")],
            ),
            (
                "[uint, int, str]",
                vec![unsigned(7), signed(9), text("boom")],
            ),
        ] {
            let parsed = extract_validation_args(args)
                .unwrap_or_else(|e| panic!("{label} must be accepted, got '{e}'"));
            assert_eq!(7, parsed.expected_min, "{label}");
        }

        // Signed values must keep working
        let parsed = extract_validation_args(vec![signed(1), text("boom")])
            .expect("[int, str] must be accepted");
        assert_eq!(1, parsed.expected_min);
        assert_eq!(1, parsed.expected_max);
        assert_eq!("boom", parsed.custom_err_msg);

        // Garbage is still rejected
        assert!(extract_validation_args(vec![]).is_err());
        assert!(extract_validation_args(vec![text("nope")]).is_err());
    }

    #[cfg(feature = "cql")]
    fn test_context() -> Context {
        Context::new(
            None,
            501,
            "dc".to_string(),
            "rack".to_string(),
            0,
            RetryInterval::new("1,2").expect("failed to parse retry interval"),
            ValidationStrategy::Ignore,
        )
    }

    #[cfg(all(feature = "alternator", not(feature = "cql")))]
    fn test_context() -> Context {
        Context::new(
            None,
            0,
            RetryInterval::new("1,2").expect("failed to parse retry interval"),
            ValidationStrategy::Ignore,
            0,
        )
    }

    #[test]
    fn clone_error_names_unserializable_data_entries() {
        #[derive(rune::Any)]
        struct FileLike;

        let mut original = test_context();
        let mut obj = rune::runtime::Object::new();
        obj.insert(
            rune::alloc::String::try_from("count").unwrap(),
            rune::runtime::Value::from(7i64),
        )
        .unwrap();
        obj.insert(
            rune::alloc::String::try_from("file").unwrap(),
            rune::runtime::Value::new(FileLike).unwrap(),
        )
        .unwrap();
        original.data = rune::runtime::Value::new(obj).unwrap();

        let message = match original.clone() {
            Err(e) => e.to_string(),
            Ok(_) => panic!("clone unexpectedly succeeded"),
        };
        assert!(message.contains("data.file"), "got: {message}");
        assert!(!message.contains("data.count"), "got: {message}");
        assert!(message.contains("plain values"), "got: {message}");
    }

    #[test]
    fn per_cycle_metrics_rejected_in_prepare_worker() {
        let mut worker = test_context().clone().unwrap();
        assert!(reject_in_setup(&worker, "record_metric").is_ok());

        // While prepare_worker runs, per-cycle stats would be cleared before
        // the run starts, so recording them must fail loudly instead.
        worker.in_prepare_worker = true;
        assert!(reject_in_setup(&worker, "record_metric").is_err());
        // The flag must survive the shallow clone handed to the script.
        assert!(worker.shallow_clone().in_prepare_worker);
    }

    #[test]
    fn worker_identity_defaults_and_propagates() {
        let original = test_context();
        assert_eq!(original.worker_id, 0);
        assert_eq!(original.worker_count, 1);

        let mut worker = original.clone().unwrap();
        worker.worker_id = 3;
        worker.worker_count = 8;
        assert_eq!(worker.shallow_clone().worker_id, 3);
        assert_eq!(worker.shallow_clone().worker_count, 8);
    }

    #[test]
    fn worker_clone_flag_propagates() {
        let original = test_context();
        assert!(!original.is_worker_clone);
        assert!(!original.shallow_clone().is_worker_clone);

        let worker = original.clone().unwrap();
        assert!(worker.is_worker_clone);
        assert!(worker.shallow_clone().is_worker_clone);
    }

    #[test]
    fn report_state_writes_rejected_in_worker_clone() {
        let original = test_context();
        assert!(reject_in_workload(&original, "set_report_field").is_ok());
        assert!(reject_in_workload(&original, "declare_metric").is_ok());

        let worker = original.clone().unwrap();
        assert!(reject_in_workload(&worker, "set_report_field").is_err());
        assert!(reject_in_workload(&worker, "declare_metric").is_err());
    }

    /// Journey-like blobs of 1..=3 KiB from consecutive seeds, laid out back to back,
    /// like the blob cells of consecutive rows in an sstable.
    fn compressible_blobs_stream(
        first_seed: i64,
        total_len: usize,
        repeat_pct: u32,
        alphabet_size: u32,
    ) -> Vec<u8> {
        let mut stream = Vec::with_capacity(total_len + 3072);
        let mut seed = first_seed;
        while stream.len() < total_len {
            let len = 1024 + (hash_inner(seed) % 2049) as usize;
            stream.extend(compressible_blob_inner(
                seed,
                len,
                repeat_pct,
                alphabet_size,
            ));
            seed += 1;
        }
        stream
    }

    /// Savings, in percent, of compressing 'data' in the 4 KiB chunks ScyllaDB uses by default
    fn chunked_savings_pct(data: &[u8], mut compress: impl FnMut(&[u8]) -> usize) -> f64 {
        let compressed: usize = data.chunks(4096).map(&mut compress).sum();
        100.0 * (1.0 - compressed as f64 / data.len() as f64)
    }

    /// Savings, in percent, of LZ4 and zstd level 3 on 1 MiB of compressible blobs
    fn compressible_blob_savings(repeat_pct: u32, alphabet_size: u32) -> (f64, f64) {
        let data = compressible_blobs_stream(0, 1 << 20, repeat_pct, alphabet_size);
        let lz4 = chunked_savings_pct(&data, |c| lz4_flex::block::compress(c).len());
        let zstd = chunked_savings_pct(&data, |c| zstd::bulk::compress(c, 3).unwrap().len());
        (lz4, zstd)
    }

    /// Savings, in percent, of zstd level 3 on 1 MiB of compressible blobs, with a dictionary
    /// trained on other blobs of the same parameters, like ScyllaDB's 'ZstdWithDictsCompressor'
    fn compressible_blob_savings_with_dict(repeat_pct: u32, alphabet_size: u32) -> f64 {
        let data = compressible_blobs_stream(0, 1 << 20, repeat_pct, alphabet_size);
        let samples: Vec<Vec<u8>> =
            compressible_blobs_stream(1_000_000, 1 << 20, repeat_pct, alphabet_size)
                .chunks(4096)
                .map(<[u8]>::to_vec)
                .collect();
        let dict = zstd::dict::from_samples(&samples, 64 * 1024).expect("failed to train");
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        chunked_savings_pct(&data, |c| compressor.compress(c).unwrap().len())
    }

    /// Prints the savings of the compressors for a grid of the 'compressible_blob' parameters.
    /// Run it to pick the parameters for a wanted compression ratio:
    ///   cargo test compressible_blob_calibration_grid -- --ignored --nocapture
    #[test]
    #[ignore]
    fn compressible_blob_calibration_grid() {
        println!("repeat_pct alphabet_size    lz4   zstd zstd+dict");
        for repeat_pct in [0, 20, 40, 50, 60, 64, 70, 80] {
            for alphabet_size in [256, 128, 64, 48, 40, 32, 16] {
                let (lz4, zstd) = compressible_blob_savings(repeat_pct, alphabet_size);
                // NOTE: there is nothing to train a dictionary on in random data
                let zstd_dict = if repeat_pct == 0 && alphabet_size == 256 {
                    zstd
                } else {
                    compressible_blob_savings_with_dict(repeat_pct, alphabet_size)
                };
                println!(
                    "{repeat_pct:>10} {alphabet_size:>13} {lz4:>6.1} {zstd:>6.1} {zstd_dict:>9.1}"
                );
            }
        }
    }

    #[test]
    fn compressible_blob_is_deterministic_and_of_given_length() {
        for len in [0, 1, 7, 8, 65, 1000, 100_000] {
            let a = compressible_blob_inner(42, len, 64, 40);
            assert_eq!(len, a.len());
            assert_eq!(a, compressible_blob_inner(42, len, 64, 40), "len {len}");
        }
        assert_ne!(
            compressible_blob_inner(42, 1000, 64, 40),
            compressible_blob_inner(43, 1000, 64, 40),
        );
    }

    #[test]
    fn compressible_blob_literals_use_the_alphabet_only() {
        for alphabet_size in [1, 2, 40, 256] {
            let values: std::collections::HashSet<u8> =
                compressible_blob_inner(7, 100_000, 0, alphabet_size)
                    .into_iter()
                    .collect();
            let expected: std::collections::HashSet<u8> = (0..alphabet_size)
                .map(|v| (v * 256 / alphabet_size) as u8)
                .collect();
            assert_eq!(expected, values, "alphabet_size {alphabet_size}");
        }
    }

    #[test]
    fn compressible_blob_without_repeats_with_full_alphabet_is_incompressible() {
        let (lz4, zstd) = compressible_blob_savings(0, 256);
        assert!(lz4 < 1.0, "lz4 saved {lz4:.1}%");
        assert!(zstd < 1.0, "zstd saved {zstd:.1}%");
    }

    /// 'repeat_pct' must drive the savings of LZ4 and zstd alike, while 'alphabet_size' must add
    /// savings for zstd only
    #[test]
    fn compressible_blob_parameters_split_lz4_and_zstd_savings() {
        let (lz4_low_repeat, _) = compressible_blob_savings(20, 256);
        let (lz4_high_repeat, _) = compressible_blob_savings(60, 256);
        assert!(
            lz4_high_repeat > lz4_low_repeat + 20.0,
            "{lz4_low_repeat:.1}% -> {lz4_high_repeat:.1}%"
        );

        let (lz4_full, zstd_full) = compressible_blob_savings(60, 256);
        let (lz4_small, zstd_small) = compressible_blob_savings(60, 32);
        assert!(
            (lz4_small - lz4_full).abs() < 1.0,
            "lz4: {lz4_full:.1}% -> {lz4_small:.1}%"
        );
        assert!(
            zstd_small > zstd_full + 10.0,
            "zstd: {zstd_full:.1}% -> {zstd_small:.1}%"
        );
    }

    /// Keeps the example of the README in sync with the generator
    #[test]
    fn compressible_blob_readme_example_savings() {
        let (lz4, _) = compressible_blob_savings(64, 40);
        let zstd_dict = compressible_blob_savings_with_dict(64, 40);
        assert!((lz4 - 49.0).abs() < 1.5, "lz4 saved {lz4:.1}%");
        assert!(
            (zstd_dict - 65.0).abs() < 1.5,
            "zstd with a dictionary saved {zstd_dict:.1}%"
        );
    }

    #[test]
    fn compressible_blob_args_are_validated() {
        assert!(check_compressible_blob_args(0, 1).is_ok());
        assert!(check_compressible_blob_args(100, 256).is_ok());
        assert!(check_compressible_blob_args(-1, 40).is_err());
        assert!(check_compressible_blob_args(101, 40).is_err());
        assert!(check_compressible_blob_args(64, 0).is_err());
        assert!(check_compressible_blob_args(64, 257).is_err());
    }

    /// Calls 'compressible_blob' from a rune script, the way workloads do
    fn call_compressible_blob(args: &str) -> Result<Value, VmError> {
        let mut context = rune::Context::with_default_modules().unwrap();
        crate::scripting::install(&mut context, HashMap::new());
        let mut sources = rune::Sources::new();
        let script = format!("pub fn main() {{ latte::compressible_blob({args}) }}");
        sources
            .insert(rune::Source::memory(script).unwrap())
            .unwrap();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let mut vm = rune::Vm::new(
            std::sync::Arc::new(context.runtime().unwrap()),
            std::sync::Arc::new(unit),
        );
        vm.call(["main"], ())
    }

    #[test]
    fn compressible_blob_is_callable_from_rune() {
        let value = call_compressible_blob("7, 1000, 64, 40").expect("failed to call");
        let bytes: Vec<u8> = rune::from_value(value).expect("not a byte vector");
        assert_eq!(compressible_blob_inner(7, 1000, 64, 40), bytes);

        assert!(call_compressible_blob("7, 1000, 101, 40").is_err());
        assert!(call_compressible_blob("7, 1000, 64, 0").is_err());
    }

    #[test]
    fn record_metric_rejected_in_setup_context() {
        let original = test_context();
        assert!(reject_in_setup(&original, "record_metric").is_err());

        let worker = original.clone().unwrap();
        assert!(reject_in_setup(&worker, "record_metric").is_ok());
    }
}
