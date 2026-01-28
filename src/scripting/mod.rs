use rune::{ContextError, Module};
use rust_embed::RustEmbed;
use std::collections::HashMap;

pub mod cluster_info;
mod functions_common;
pub mod retry_error;
mod row_distribution;
pub mod rune_uuid;
mod split_lines_iter;

#[cfg(feature = "alternator")]
mod alternator;
#[cfg(feature = "cql")]
mod cql;

#[cfg(feature = "cql")]
pub use cql::cass_error as db_error;
#[cfg(feature = "cql")]
pub use cql::connect;
#[cfg(feature = "cql")]
pub use cql::context;

#[cfg(feature = "alternator")]
pub use alternator::alternator_error as db_error;
#[cfg(feature = "alternator")]
pub use alternator::connect;
#[cfg(feature = "alternator")]
pub use alternator::context;

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
    use cql::cql_types;
    use cql::functions;

    let mut context_module = init_context_module()?;
    context_module.function_meta(functions::prepare)?;

    // NOTE: 1st group of query-oriented functions - without usage of prepared statements
    context_module.function_meta(functions::execute)?;
    context_module.function_meta(functions::execute_with_validation)?;
    context_module.function_meta(functions::execute_with_result)?;
    // NOTE: 2nd group of query-oriented functions - with usage of prepared statements
    context_module.function_meta(functions::execute_prepared)?;
    context_module.function_meta(functions::execute_prepared_with_validation)?;
    context_module.function_meta(functions::execute_prepared_with_result)?;

    context_module.function_meta(functions::batch_prepared)?;
    context_module.function_meta(functions::get_datacenters)?;

    let err_module = init_error_module()?;
    let uuid_module = init_uuid_module()?;
    let mut latte_module = init_latte_module(params)?;

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

    let mut fs_module = init_fs_module()?;
    let iter_module = init_iter_module(&mut fs_module)?;

    rune_ctx.install(&context_module)?;
    rune_ctx.install(&err_module)?;
    rune_ctx.install(&uuid_module)?;
    rune_ctx.install(&latte_module)?;
    rune_ctx.install(&fs_module)?;
    rune_ctx.install(&iter_module)?;

    Ok(())
}

#[cfg(feature = "alternator")]
fn try_install(
    rune_ctx: &mut rune::Context,
    params: HashMap<String, String>,
) -> Result<(), ContextError> {
    use alternator::functions;
    let mut context_module = init_context_module()?;
    context_module.function_meta(functions::create_table)?;
    context_module.function_meta(functions::delete_table)?;
    context_module.function_meta(functions::put)?;
    context_module.function_meta(functions::get)?;
    context_module.function_meta(functions::delete)?;
    context_module.function_meta(functions::update)?;
    context_module.function_meta(functions::query)?;
    context_module.function_meta(functions::scan)?;

    let err_module = init_error_module()?;
    let uuid_module = init_uuid_module()?;
    let latte_module = init_latte_module(params)?;
    let mut fs_module = init_fs_module()?;
    let iter_module = init_iter_module(&mut fs_module)?;

    rune_ctx.install(&context_module)?;
    rune_ctx.install(&err_module)?;
    rune_ctx.install(&uuid_module)?;
    rune_ctx.install(&latte_module)?;
    rune_ctx.install(&fs_module)?;
    rune_ctx.install(&iter_module)?;

    Ok(())
}

fn init_context_module() -> Result<Module, ContextError> {
    let mut context_module = Module::default();

    context_module.ty::<context::Context>()?;
    context_module.function_meta(functions_common::signal_failure)?;
    context_module.function_meta(functions_common::elapsed_secs)?;

    context_module.function_meta(row_distribution::init_partition_row_distribution_preset)?;
    context_module.function_meta(row_distribution::get_partition_idx)?;
    context_module.ty::<row_distribution::Partition>()?;
    context_module.function_meta(row_distribution::get_partition_info)?;

    Ok(context_module)
}

fn init_error_module() -> Result<Module, ContextError> {
    let mut err_module = Module::default();

    err_module.ty::<db_error::DbError>()?;
    err_module.function_meta(db_error::DbError::string_display)?;

    Ok(err_module)
}

fn init_uuid_module() -> Result<Module, ContextError> {
    let mut uuid_module = Module::default();

    uuid_module.ty::<rune_uuid::Uuid>()?;
    uuid_module.function_meta(rune_uuid::Uuid::string_display)?;

    Ok(uuid_module)
}

fn init_latte_module(params: HashMap<String, String>) -> Result<Module, ContextError> {
    let mut latte_module = Module::with_crate("latte")?;

    latte_module.macro_("param", move |ctx, ts| {
        functions_common::param(ctx, &params, ts)
    })?;
    latte_module.function_meta(functions_common::blob)?;
    latte_module.function_meta(functions_common::text)?;
    latte_module.function_meta(functions_common::vector)?;
    latte_module.function_meta(functions_common::join)?;
    latte_module.function_meta(functions_common::now_timestamp)?;
    latte_module.function_meta(functions_common::hash)?;
    latte_module.function_meta(functions_common::hash2)?;
    latte_module.function_meta(functions_common::hash_range)?;
    latte_module.function_meta(functions_common::hash_select)?;
    latte_module.function_meta(functions_common::uuid)?;
    latte_module.function_meta(functions_common::normal)?;
    latte_module.function_meta(functions_common::normal_f32)?;
    latte_module.function_meta(functions_common::uniform)?;
    latte_module.function_meta(functions_common::is_none)?;

    Ok(latte_module)
}

fn init_fs_module() -> Result<Module, ContextError> {
    let mut fs_module = Module::with_crate("fs")?;

    fs_module.function_meta(functions_common::read_to_string)?;
    fs_module.function_meta(functions_common::read_lines)?;
    fs_module.function_meta(functions_common::read_words)?;
    fs_module.function_meta(functions_common::read_resource_to_string)?;
    fs_module.function_meta(functions_common::read_resource_lines)?;
    fs_module.function_meta(functions_common::read_resource_words)?;

    Ok(fs_module)
}

fn init_iter_module(fs_module: &mut Module) -> Result<Module, ContextError> {
    let mut iter_module = Module::default();

    iter_module.ty::<split_lines_iter::SplitLinesIterator>()?;
    fs_module.function_meta(split_lines_iter::read_split_lines_iter)?;
    iter_module.function_meta(split_lines_iter::next)?;

    Ok(iter_module)
}
