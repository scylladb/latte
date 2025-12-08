use rune::{ContextError, Module};
use rust_embed::RustEmbed;
use std::collections::HashMap;

pub mod retry_error;
mod row_distribution;
pub mod rune_uuid;
mod split_lines_iter;

#[cfg(feature = "cql")]
mod cql;

#[cfg(feature = "cql")]
pub use cql::cass_error as db_error;
#[cfg(feature = "cql")]
pub use cql::connect;
#[cfg(feature = "cql")]
pub use cql::context;

#[derive(RustEmbed)]
#[folder = "resources/"]
struct Resources;

pub fn install(rune_ctx: &mut rune::Context, params: HashMap<String, String>) {
    try_install(rune_ctx, params).unwrap()
}

#[cfg(feature = "cql")]
fn try_install(
    rune_ctx: &mut rune::Context,
    params: HashMap<String, String>,
) -> Result<(), ContextError> {
    use cql::cass_error::CassError;
    use cql::context::Context;
    use cql::cql_types;
    use cql::functions;

    let mut context_module = Module::default();
    context_module.ty::<Context>()?;
    context_module.function_meta(functions::prepare)?;
    context_module.function_meta(functions::signal_failure)?;

    // NOTE: 1st group of query-oriented functions - without usage of prepared statements
    context_module.function_meta(functions::execute)?;
    context_module.function_meta(functions::execute_with_validation)?;
    context_module.function_meta(functions::execute_with_result)?;
    // NOTE: 2nd group of query-oriented functions - with usage of prepared statements
    context_module.function_meta(functions::execute_prepared)?;
    context_module.function_meta(functions::execute_prepared_with_validation)?;
    context_module.function_meta(functions::execute_prepared_with_result)?;

    context_module.function_meta(functions::batch_prepared)?;
    context_module.function_meta(row_distribution::init_partition_row_distribution_preset)?;
    context_module.function_meta(row_distribution::get_partition_idx)?;
    context_module.ty::<row_distribution::Partition>()?;
    context_module.function_meta(row_distribution::get_partition_info)?;
    context_module.function_meta(functions::get_datacenters)?;
    context_module.function_meta(functions::elapsed_secs)?;

    let mut err_module = Module::default();
    err_module.ty::<CassError>()?;
    err_module.function_meta(CassError::string_display)?;

    let mut uuid_module = Module::default();
    uuid_module.ty::<rune_uuid::Uuid>()?;
    uuid_module.function_meta(rune_uuid::Uuid::string_display)?;

    let mut latte_module = Module::with_crate("latte")?;
    latte_module.macro_("param", move |ctx, ts| functions::param(ctx, &params, ts))?;

    latte_module.function_meta(functions::blob)?;
    latte_module.function_meta(functions::text)?;
    latte_module.function_meta(functions::vector)?;
    latte_module.function_meta(functions::join)?;
    latte_module.function_meta(functions::now_timestamp)?;
    latte_module.function_meta(functions::hash)?;
    latte_module.function_meta(functions::hash2)?;
    latte_module.function_meta(functions::hash_range)?;
    latte_module.function_meta(functions::hash_select)?;
    latte_module.function_meta(functions::uuid)?;
    latte_module.function_meta(functions::normal)?;
    latte_module.function_meta(functions::normal_f32)?;
    latte_module.function_meta(functions::uniform)?;
    latte_module.function_meta(functions::is_none)?;

    latte_module.function_meta(cql_types::i64::to_i32)?;
    latte_module.function_meta(cql_types::i64::to_i16)?;
    latte_module.function_meta(cql_types::i64::to_i8)?;
    latte_module.function_meta(cql_types::i64::to_f32)?;
    latte_module.function_meta(cql_types::i64::clamp)?;

    latte_module.function_meta(cql_types::f64::to_i8)?;
    latte_module.function_meta(cql_types::f64::to_i16)?;
    latte_module.function_meta(cql_types::f64::to_i32)?;
    latte_module.function_meta(cql_types::f64::to_f32)?;
    latte_module.function_meta(cql_types::f64::clamp)?;

    let mut fs_module = Module::with_crate("fs")?;
    fs_module.function_meta(functions::read_to_string)?;
    fs_module.function_meta(functions::read_lines)?;
    fs_module.function_meta(functions::read_words)?;
    fs_module.function_meta(functions::read_resource_to_string)?;
    fs_module.function_meta(functions::read_resource_lines)?;
    fs_module.function_meta(functions::read_resource_words)?;
    let mut iter_module = Module::default();
    iter_module.ty::<split_lines_iter::SplitLinesIterator>()?;
    fs_module.function_meta(split_lines_iter::read_split_lines_iter)?;
    iter_module.function_meta(split_lines_iter::next)?;

    rune_ctx.install(&context_module)?;
    rune_ctx.install(&err_module)?;
    rune_ctx.install(&uuid_module)?;
    rune_ctx.install(&latte_module)?;
    rune_ctx.install(&fs_module)?;
    rune_ctx.install(&iter_module)?;

    Ok(())
}
