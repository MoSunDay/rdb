//! Shared harness for the `string` command tests: a crate-wide store
//! lock (each opened store wipes `/tmp/rdb-test-{pid}` wholesale, so
//! every test holds the lock for its whole lifetime; the guard is
//! returned FIRST so it outlives the `Shared` — locals drop in reverse
//! declaration order) plus the one-shot `call` driver.
use crate::command::test_ctx;
use crate::state::{testutil, Shared};

pub(crate) const PREFIX: &[u8] = b"70/";

pub(crate) fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = super::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

pub(crate) fn call(shared: &Shared, handler: crate::command::Handler, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}
