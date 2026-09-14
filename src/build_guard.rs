//! Compile-time guard: the `full` server stack MUST be built with
//! `--cfg tokio_unstable` (set by the committed `.cargo/config.toml`).
//! Without it the tokio multi-thread scheduler's LIFO slot is active and
//! loses task wakeups under load (freezes 6s+; see COMPAT.md "tokio
//! LIFO slot freeze"). The store-only embedder slice
//! (`--no-default-features --features store`) never spawns the rdb
//! runtime and is exempt.

#[cfg(all(feature = "full", not(tokio_unstable)))]
compile_error!(
    "rdb 'full' build requires --cfg tokio_unstable: the tokio LIFO-slot \
     workaround would be inactive (see .cargo/config.toml and COMPAT.md \
     'tokio LIFO slot freeze'). Build with the committed .cargo/config.toml \
     or set RUSTFLAGS='--cfg tokio_unstable'. The store-only slice \
     (--no-default-features --features store) is exempt."
);
