//! Static command metadata: the single source of truth for `COMMAND`.
//!
//! One [`CmdMeta`] per command NAME (aliases such as DEL/UNLINK get their
//! own rows). Conventions are real Redis': `arity` is the exact argc when
//! positive and the MINIMUM argc when negative (argc counts the command
//! name itself); `first_key`/`last_key`/`step` are 1-based positions into
//! argv INCLUDING the command name, `last_key < 0` counts back from the
//! end, and `0,0,0` marks keyless commands. Exotic/queue-ish commands
//! (streams, json, vector sets, ft.*) carry `1,1,1` placeholders.

/// Wire-visible metadata of one command (the `COMMAND` reply fields).
pub struct CmdMeta {
    pub name: &'static str,
    pub arity: i64,
    pub first_key: i64,
    pub last_key: i64,
    pub step: i64,
}

mod table;

pub use table::COMMANDS;

/// Case-insensitive lookup by raw command name bytes.
pub fn lookup_meta(name: &[u8]) -> Option<&'static CmdMeta> {
    COMMANDS
        .iter()
        .find(|meta| eq_ignore_case(name, meta.name.as_bytes()))
}

/// ASCII case-insensitive equality (command names are ASCII lowercase).
fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

#[cfg(test)]
mod tests;
