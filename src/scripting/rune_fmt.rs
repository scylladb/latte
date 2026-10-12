//! Formatting of rune values for error messages

use crate::scripting::rune_uuid::Uuid;
use rune::Value;

/// Nesting depth after which `fmt_rune_value` stops descending,
/// so that self-referencing containers cannot recurse forever.
const MAX_FMT_DEPTH: usize = 16;

/// Formats a rune Value for error messages.
///
/// Rune's own `Debug` impl (`Value::debug_fmt`) resolves the formatting of heap values
/// through the thread-local VM environment. That environment is set only while the VM
/// executes instructions, not while it polls the futures of native async functions,
/// so any formatting done from within those functions gets
/// `<... object at 0x...: Missing interface environment>` instead of the value.
/// This function handles the value types that workloads pass to the database
/// on its own and falls back to the type name for everything else.
pub fn fmt_rune_value(v: &Value) -> String {
    let mut buf = String::new();
    write_rune_value(&mut buf, v, 0);
    buf
}

fn write_rune_value(buf: &mut String, v: &Value, depth: usize) {
    use rune::runtime::TypeValue;
    use std::fmt::Write;

    if depth > MAX_FMT_DEPTH {
        buf.push_str("...");
        return;
    }
    if let Ok(b) = v.as_bool() {
        let _ = write!(buf, "{b}");
        return;
    }
    if let Ok(c) = v.as_char() {
        let _ = write!(buf, "{c:?}");
        return;
    }
    if let Ok(u) = v.as_unsigned() {
        let _ = write!(buf, "{u}");
        return;
    }
    if let Ok(i) = v.as_signed() {
        let _ = write!(buf, "{i}");
        return;
    }
    if let Ok(f) = v.as_float() {
        let _ = write!(buf, "{f:?}");
        return;
    }
    match v.as_type_value() {
        Ok(TypeValue::Unit) => {
            buf.push_str("()");
            return;
        }
        Ok(TypeValue::Tuple(tuple)) => {
            write_rune_seq(buf, "(", tuple.iter(), ")", depth);
            return;
        }
        Ok(TypeValue::Object(obj)) => {
            buf.push('{');
            for (i, (k, v)) in obj.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                let _ = write!(buf, "{k}: ");
                write_rune_value(buf, v, depth + 1);
            }
            buf.push('}');
            return;
        }
        Ok(TypeValue::EmptyStruct(s)) => {
            let _ = write!(buf, "{}", s.rtti().item());
            return;
        }
        Ok(TypeValue::TupleStruct(s)) => {
            let _ = write!(buf, "{}", s.rtti().item());
            write_rune_seq(buf, "(", s.data().iter(), ")", depth);
            return;
        }
        Ok(TypeValue::Struct(s)) => {
            // Rune doesn't expose struct field names, only their values in declaration order
            let _ = write!(buf, "{} ", s.rtti().item());
            write_rune_seq(buf, "{ ", s.data().iter(), " }", depth);
            return;
        }
        _ => {}
    }
    if let Ok(opt) = v.borrow_ref::<Option<Value>>() {
        match opt.as_ref() {
            Some(inner) => {
                buf.push_str("Some(");
                write_rune_value(buf, inner, depth + 1);
                buf.push(')');
            }
            None => buf.push_str("None"),
        }
        return;
    }
    if let Ok(s) = v.borrow_ref::<rune::alloc::String>() {
        let _ = write!(buf, "{:?}", s.as_str());
        return;
    }
    if let Ok(b) = v.borrow_ref::<rune::runtime::Bytes>() {
        let _ = write!(buf, "{:?}", b.as_slice());
        return;
    }
    if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
        write_rune_seq(buf, "[", vec.iter(), "]", depth);
        return;
    }
    if let Ok(uuid) = v.borrow_ref::<Uuid>() {
        let _ = write!(buf, "{}", uuid.0);
        return;
    }
    #[cfg(feature = "cql")]
    if write_cql_type_value(buf, v) {
        return;
    }
    let _ = write!(buf, "<{}>", v.type_info());
}

fn write_rune_seq<'a>(
    buf: &mut String,
    open: &str,
    items: impl Iterator<Item = &'a Value>,
    close: &str,
    depth: usize,
) {
    buf.push_str(open);
    for (i, v) in items.enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        write_rune_value(buf, v, depth + 1);
    }
    buf.push_str(close);
}

/// Writes the values of latte's CQL-specific numeric types, returns whether `v` was one of them
#[cfg(feature = "cql")]
fn write_cql_type_value(buf: &mut String, v: &Value) -> bool {
    use super::cql::cql_types::{Float32, Int16, Int32, Int8};
    use std::fmt::Write;

    if let Ok(x) = v.borrow_ref::<Int8>() {
        let _ = write!(buf, "{}", x.0);
    } else if let Ok(x) = v.borrow_ref::<Int16>() {
        let _ = write!(buf, "{}", x.0);
    } else if let Ok(x) = v.borrow_ref::<Int32>() {
        let _ = write!(buf, "{}", x.0);
    } else if let Ok(x) = v.borrow_ref::<Float32>() {
        let _ = write!(buf, "{:?}", x.0);
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use rune::alloc::String as RuneString;
    use rune::runtime::{Bytes, Object, OwnedTuple, Vec as RuneVec};
    use rune::{Context, Diagnostics, Source, Sources, Vm};
    use std::sync::Arc;

    // Unit tests run with no rune VM environment set up, the same as code polled
    // from within native async functions, which is where the error messages are built.

    fn rune_string(s: &str) -> Value {
        Value::new(RuneString::try_from(s).unwrap()).unwrap()
    }

    fn rune_vec(items: Vec<Value>) -> Value {
        let mut v = RuneVec::new();
        for item in items {
            v.push(item).unwrap();
        }
        Value::vec(v.into_inner()).unwrap()
    }

    fn rune_tuple(items: Vec<Value>) -> Value {
        let mut v = rune::alloc::vec::Vec::new();
        for item in items {
            v.try_push(item).unwrap();
        }
        Value::new(OwnedTuple::try_from(v).unwrap()).unwrap()
    }

    fn rune_object(pairs: Vec<(&str, Value)>) -> Value {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(RuneString::try_from(k).unwrap(), v).unwrap();
        }
        Value::new(obj).unwrap()
    }

    /// Evaluates a rune expression and returns its value once the VM is done with it
    fn eval_rune(script: &str) -> Value {
        let context = Context::with_default_modules().unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        let mut vm = Vm::new(Arc::new(context.runtime().unwrap()), Arc::new(unit));
        vm.call(["main"], ()).unwrap()
    }

    #[test]
    fn test_fmt_rune_value_scalars() {
        assert_eq!(fmt_rune_value(&Value::from(true)), "true");
        assert_eq!(fmt_rune_value(&Value::from('x')), "'x'");
        assert_eq!(fmt_rune_value(&Value::from(7u64)), "7");
        assert_eq!(fmt_rune_value(&Value::from(-42i64)), "-42");
        assert_eq!(fmt_rune_value(&Value::from(1.5f64)), "1.5");
        assert_eq!(fmt_rune_value(&Value::from(2.0f64)), "2.0");
        assert_eq!(fmt_rune_value(&Value::from(())), "()");
    }

    #[test]
    fn test_fmt_rune_value_string() {
        assert_eq!(fmt_rune_value(&rune_string("hello")), "\"hello\"");
        assert_eq!(fmt_rune_value(&rune_string("a\"b")), "\"a\\\"b\"");
    }

    #[test]
    fn test_fmt_rune_value_bytes() {
        let bytes = Value::new(Bytes::from_vec(vec![1u8, 2, 255].try_into().unwrap())).unwrap();
        assert_eq!(fmt_rune_value(&bytes), "[1, 2, 255]");
    }

    #[test]
    fn test_fmt_rune_value_option() {
        let some = Value::new(Some(rune_string("x"))).unwrap();
        let none = Value::new(None::<Value>).unwrap();
        assert_eq!(fmt_rune_value(&some), "Some(\"x\")");
        assert_eq!(fmt_rune_value(&none), "None");
    }

    #[test]
    fn test_fmt_rune_value_containers() {
        let vec = rune_vec(vec![Value::from(1i64), rune_string("a")]);
        assert_eq!(fmt_rune_value(&vec), "[1, \"a\"]");
        let tuple = rune_tuple(vec![rune_string("k"), vec]);
        assert_eq!(fmt_rune_value(&tuple), "(\"k\", [1, \"a\"])");
        let obj = rune_object(vec![("name", rune_string("v"))]);
        assert_eq!(fmt_rune_value(&obj), "{name: \"v\"}");
    }

    #[test]
    fn test_fmt_rune_value_uuid() {
        let uuid = Value::new(Uuid(uuid::Uuid::nil())).unwrap();
        assert_eq!(
            fmt_rune_value(&uuid),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[cfg(feature = "cql")]
    #[test]
    fn test_fmt_rune_value_cql_types() {
        use crate::scripting::cql::cql_types::{Float32, Int16, Int32, Int8};

        assert_eq!(fmt_rune_value(&Value::new(Int8(-8)).unwrap()), "-8");
        assert_eq!(fmt_rune_value(&Value::new(Int16(16)).unwrap()), "16");
        assert_eq!(fmt_rune_value(&Value::new(Int32(32)).unwrap()), "32");
        assert_eq!(fmt_rune_value(&Value::new(Float32(0.5)).unwrap()), "0.5");
    }

    #[test]
    fn test_fmt_rune_value_script_structs() {
        let value = eval_rune(
            r#"
            struct Empty;
            struct Pair(a, b);
            struct Row { id, name }
            pub fn main() {
                [Empty, Pair(1, "x"), Row { id: 2, name: "y" }]
            }
            "#,
        );
        assert_eq!(
            fmt_rune_value(&value),
            "[Empty, Pair(1, \"x\"), Row { 2, \"y\" }]"
        );
    }

    #[test]
    fn test_fmt_rune_value_self_reference_is_bounded() {
        let value = eval_rune("pub fn main() { let v = []; v.push(v); v }");
        let formatted = fmt_rune_value(&value);
        assert!(formatted.contains("..."), "got: {formatted}");
    }
}
