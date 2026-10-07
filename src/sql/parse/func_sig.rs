//! Scalar-function signature table: pure name -> arity bounds, checked
//! at translate time so a bad parameter count fails at prepare (MySQL
//! 1582 "Incorrect parameter count in the call to native function")
//! instead of at the first row of execution.
//!
//! Entries must stay in lockstep with the evaluator families in
//! `exec/func/` (a name here the evaluator does not own still evaluates
//! to `unknown function`; a family entry missing here only loses the
//! early arity error) and with the result-type table in `func::meta`.
//! Special SQL forms translate into these same plain calls: SUBSTRING
//! (FROM/FOR, SUBSTR), TRIM (BOTH/LEADING/TRAILING .. FROM), POSITION
//! (.. IN .. -> locate) and DATE_ADD's INTERVAL (3-arg desugar) -- see
//! `parse::func_forms`.

use crate::sql::exec::func;
use crate::sql::parse::error::SqlResult;

/// One signature: `(min_args, max_args)`; `usize::MAX` marks variadic
/// (no variadic entry yet -- part B adds the first ones).
type Sig = (usize, usize);

/// Known scalar signatures (lowercase names, as the translator
/// normalizes them). Aggregates (COUNT/SUM/AVG/MIN/MAX) are handled by
/// their own IR node before this table is consulted.
/// `usize::MAX` upper bound = variadic (CONCAT and friends).
const VAR: usize = usize::MAX;

fn signature(name: &str) -> Option<Sig> {
    Some(match name {
        "length" | "char_length" | "upper" | "lower" | "abs" => (1, 1),
        "version" => (0, 0),
        // Clock functions: the optional fsp argument parses (ignored).
        "now" | "current_timestamp" | "sysdate" | "localtime" | "localtimestamp" => (0, 1),
        "curdate" | "current_date" | "curtime" | "current_time" => (0, 1),
        // LAST_INSERT_ID(): 0 or 1 argument (the value form sets it).
        "last_insert_id" => (0, 1),
        // Session scalar functions (M4): bound per execution by
        // `exec::session_funcs` -- zero arguments by MySQL's grammar.
        "database" | "schema" | "user" | "current_user" | "session_user" => (0, 0),
        "connection_id" => (0, 0),
        // Lazy control family (evaluated before arguments; the arity
        // shape is still checked here so prepare-time errors match).
        "if" => (3, 3),
        "ifnull" | "nullif" => (2, 2),
        "coalesce" => (1, VAR),
        // String family.
        "concat" => (1, VAR),
        "concat_ws" => (2, VAR),
        "substring" => (2, 3), // SUBSTR + the FROM/FOR spellings
        "left" | "right" | "repeat" | "instr" => (2, 2),
        "lpad" | "rpad" | "replace" => (3, 3),
        // TRIM: 1-arg plain form or the translated 3-arg
        // (s, remstr, mode-literal) form; the evaluator rejects 2.
        "trim" => (1, 3),
        "locate" => (2, 3), // POSITION(.. IN ..) lands here too
        "reverse" | "hex" | "unhex" => (1, 1),
        // Numeric family.
        "round" => (1, 2),
        "ceil" | "ceiling" | "floor" | "sqrt" | "sign" => (1, 1),
        "truncate" | "mod" | "pow" | "power" => (2, 2),
        "greatest" | "least" => (1, VAR),
        // Datetime family. The INTERVAL entries always desugar to
        // (date, n, unit); ADDDATE/SUBDATE also take plain days.
        "date" | "year" | "month" | "day" | "dayofmonth" | "hour" | "minute" | "second" => (1, 1),
        "date_add" | "date_sub" => (3, 3),
        "adddate" | "subdate" => (2, 3),
        "datediff" | "date_format" => (2, 2),
        "from_unixtime" => (1, 2), // the format-argument spelling
        "unix_timestamp" => (0, 1),
        _ => return None,
    })
}

/// Arity check of one call: `Some(Ok(()))` = known name, arity fine;
/// `Some(Err(..))` = known name, wrong parameter count; `None` = the
/// name is not in the table (unknown functions keep their runtime
/// "unknown function" error).
pub(crate) fn check_signature(name: &str, argc: usize) -> Option<SqlResult<()>> {
    signature(name).map(|(min, max)| {
        if argc >= min && argc <= max {
            Ok(())
        } else {
            Err(func::wrong_param_count(name))
        }
    })
}

#[cfg(test)]
#[path = "func_sig_tests.rs"]
mod tests;
