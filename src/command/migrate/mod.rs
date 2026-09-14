//! `MIGRATE` (data transport + admin tasks) and `RESTORE`.
//!
//! Data plane: `MIGRATE host port key db timeout [COPY] [REPLACE]
//! [KEYS key ...]` dumps each key on THIS node and transports it to the
//! target with `ASKING` + `RESTORE` (the target's IMPORTING table serves
//! only ASKING connections). The dump wire format lives in
//! `crate::ds::dump`; the outbound RESP client in `crate::resp::client`.
//! Admin plane (rdb extension): `migrate task <slot> <src> <dst>` drives
//! the full redis-cli `--cluster reshard` protocol against the two nodes
//! over the outbound client (src MIGRATING -> dst IMPORTING -> drain via
//! GETKEYSINSLOT + MIGRATE -> NODE on both -> STABLE). The one current
//! task (JSON) replicates through the raft key `migrate_task`; `migrate
//! list` returns it.
//!
//! Layout: this module only dispatches (`handle`) and holds the shared
//! usage helper; the data plane lives in `data`, the admin plane in
//! `task`.

pub mod data;
pub mod task;

pub use data::restore;

use crate::command::Ctx;
use crate::resp::codec::append_error;

/// `MIGRATE ...` dispatch; lowercase-only entries, exactly as Go's map.
pub async fn handle(ctx: &mut Ctx<'_>) {
    let Some(first) = ctx.args.first() else {
        migrate_helper(ctx);
        return;
    };
    match first.as_slice() {
        b"help" => migrate_helper(ctx),
        b"task" => task::migrate_task(ctx).await,
        b"list" => task::migrate_list(ctx),
        // Anything else is the DATA command: `MIGRATE host port key db
        // timeout ...` (first arg = host).
        _ => data::migrate_data(ctx).await,
    }
}

/// Go quirk kept: the usage message is an ERROR reply.
fn migrate_helper(ctx: &mut Ctx<'_>) {
    append_error(ctx.out, "migrate [ list | task ]");
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{call, shared_for};

    #[test]
    fn helper_is_an_error_reply() {
        let (_guard, shared) = shared_for("127.0.0.1:40401");
        let helper = b"-migrate [ list | task ]\r\n";
        assert_eq!(call(&shared, &[]), helper);
        assert_eq!(call(&shared, &[b"help"]), helper);
        // Unknown subcommands fall through to the DATA migrate: a lone
        // arg is an arity error (Redis text).
        assert_eq!(
            call(&shared, &[b"bogus"]),
            b"-ERR wrong number of arguments for 'migrate' command\r\n"
        );
        // Uppercase does not match the lowercase-only registry, so "TASK"
        // also falls through to the DATA command (arity error).
        assert_eq!(
            call(&shared, &[b"TASK"]),
            b"-ERR wrong number of arguments for 'migrate' command\r\n"
        );
    }
}
