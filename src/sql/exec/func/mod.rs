//! Scalar-function dispatch: pure per-family evaluation, chained in a
//! fixed order. No registry struct, no state -- a family module owns a
//! set of function names and answers with `None` only when the name is
//! not its business, so `eval_func` below is the single evaluation
//! entry the expression executor calls.
//!
//! Structural nodes that need lazy, scope-based evaluation (CASE short
//! circuits before evaluating a THEN; CAST evaluates its operand first)
//! live in [`control`] as dedicated helpers instead of the value-level
//! `eval(name, args)` shape, as do the lazy control functions
//! (IF/IFNULL/NULLIF/COALESCE), which the expression executor must
//! intercept BEFORE argument evaluation. [`meta`] holds the pure
//! result-type table shared with the projection typer.

mod control;
mod datetime;
mod datetime_more;
pub(crate) mod meta;
mod numeric;
mod numeric_more;
mod string;
mod string_more;

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

// Structural helpers the expression executor calls directly (they are
// not name-dispatched like the functions above).
pub(crate) use control::{eval_case, eval_cast, eval_lazy};
pub(crate) use numeric::eval_bitop;
pub(crate) use string::{regexp_match, value_text};

/// Evaluate scalar function `name` over already-evaluated `args`.
pub fn eval_func(name: &str, args: &[Value]) -> SqlResult<Value> {
    // Fixed chain; the first family owning the name answers, even with
    // an error (arity/type) -- later families never see it.
    control::eval(name, args)
        .or_else(|| string::eval(name, args))
        .or_else(|| numeric::eval(name, args))
        .or_else(|| datetime::eval(name, args))
        .unwrap_or_else(|| Err(unknown(name)))
}

/// MySQL-style unknown-function error (the pre-parse signature table
/// in `parse::func_sig` rejects bad arity for known names earlier).
fn unknown(name: &str) -> SqlError {
    SqlError::new(ErrorCode::NotSupported, format!("unknown function {name}"))
}

/// MySQL 1582: "Incorrect parameter count in the call to native
/// function 'x'" -- shared by the family evaluators and the
/// translate-time signature table so both ends agree.
pub(crate) fn wrong_param_count(name: &str) -> SqlError {
    SqlError::new(
        ErrorCode::WrongParamCount,
        format!("Incorrect parameter count in the call to native function '{name}'"),
    )
}

#[cfg(test)]
mod tests {
    use super::eval_func;
    use crate::sql::storage::schema::Value;

    // The chain owns nothing else: an unregistered name stays unknown,
    // and a known name with bad arity is the family's loud error.
    #[test]
    fn dispatch_owns_names_exclusively() {
        assert!(eval_func("no_such_fn", &[]).is_err());
        let e = eval_func("upper", &[Value::Null, Value::Null]).unwrap_err();
        assert!(e.msg.contains("Incorrect parameter count"), "msg: {e}");
        assert!(matches!(eval_func("version", &[]), Ok(Value::Str(_))));
    }
}
