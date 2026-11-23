//! Multi-store scenario #6 -- a delayed-job scheduler on the lite MQ:
//! jobs are STAGED with `XADD ... DELAY <ms>` and exchanged by the
//! spawned due sweep (`lite.delay_sweep_ms`, lite_delay_proc_e2e), each
//! execution is guarded by a `SET NX PX` lock, the SQL ledger tracks
//! the state machine (pending -> running -> done / dead), and a job
//! whose delivery count exceeds the group's MAXDELIVERY lands in the
//! stream DLQ (lite_dlq_e2e transfer semantics) -- reconciled against
//! the ledger at the end.

use crate::common;
use crate::scenario_common;

use common::mysql::{connect_root, ddl, one, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, ok, r, rb, rf, until_contains, FrontSpec};

/// Sweep rhythm armed in the node's yaml.
const SWEEP_MS: u64 = 200;

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    }
}

#[tokio::test]
async fn delayed_jobs_lock_ledger_and_dlq() {
    let _serial = scenario_common::serial_guard().await;
    let (front, a) = front_world("jobs6", &spec()).await;
    let mut c = connect_root(&front.node).await;

    // a) The ledger: every staged job starts `pending` with 0 tries.
    ddl(
        &mut c,
        "CREATE TABLE job_ledger (job VARCHAR(64) PRIMARY KEY, state VARCHAR(16) NOT NULL, tries INT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO job_ledger (job, state, tries) VALUES \
         ('email-welcome', 'pending', 0), ('report-nightly', 'pending', 0), ('cleanup', 'pending', 0)")
    .await;

    // b) Stage three jobs: two due soon, one far out. Delayed rows are
    //    invisible to every reader until exchanged.
    for job in ["email-welcome", "report-nightly"] {
        let reply = r(
            &a,
            &[
                b"XADD",
                b"jobs/todo",
                b"*",
                b"DELAY",
                b"600",
                b"job",
                job.as_bytes(),
            ],
        )
        .await;
        assert!(
            scenario_common::bulk_at(&reply, 0).contains('-'),
            "staged {job}"
        );
    }
    let reply = r(
        &a,
        &[
            b"XADD",
            b"jobs/todo",
            b"*",
            b"DELAY",
            b"60000",
            b"job",
            b"cleanup",
        ],
    )
    .await;
    assert!(
        scenario_common::bulk_at(&reply, 0).contains('-'),
        "staged cleanup"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"jobs/todo"]).await),
        0,
        "nothing due yet"
    );
    let workers = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"workers",
            b"w1",
            b"STREAMS",
            b"jobs/todo",
            b">",
        ],
    )
    .await;
    assert!(
        !workers.contains("email-welcome"),
        "delayed jobs never leak: {workers}"
    );

    // c) The sweep exchanges the two short jobs; the group hands them
    //    out exactly once.
    until_contains(&a, &[b"XLEN", b"jobs/todo"], ":2", "two jobs exchanged", 15).await;
    ok(
        &r(
            &a,
            &[
                b"XGROUP",
                b"CREATE",
                b"jobs/todo",
                b"workers",
                b"0-0",
                b"MAXDELIVERY",
                b"2",
            ],
        )
        .await,
        "group",
    );
    let delivered = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"workers",
            b"w1",
            b"STREAMS",
            b"jobs/todo",
            b">",
        ],
    )
    .await;
    assert!(
        delivered.contains("email-welcome") && delivered.contains("report-nightly"),
        "{delivered}"
    );
    // The delay exchange mints FRESH ids: read them out of the delivery
    // (staging order email -> report is preserved into the stream).
    let ids = scenario_common::stream_ids(&delivered);
    assert_eq!(ids.len(), 2, "two entries delivered: {delivered}");
    let report_id = ids[1].clone();
    assert!(
        !delivered.contains("cleanup"),
        "far-future job stays staged: {delivered}"
    );

    // d) Executor takes the SET-NX-PX lease; a rival sees the nil.
    ok(
        &r(
            &a,
            &[
                b"SET",
                b"lock:job:email-welcome",
                b"exec-1",
                b"NX",
                b"PX",
                b"5000",
            ],
        )
        .await,
        "lock taken",
    );
    let stolen = r(
        &a,
        &[
            b"SET",
            b"lock:job:email-welcome",
            b"exec-2",
            b"NX",
            b"PX",
            b"5000",
        ],
    )
    .await;
    assert!(
        stolen.starts_with(b"$-1"),
        "double lease refused: {stolen:?}"
    );
    run(
        &mut c,
        "UPDATE job_ledger SET state = 'done', tries = tries + 1 WHERE job = 'email-welcome'",
    )
    .await;
    // The report job's executor dies: claim (delivery 2) then over-limit
    // claim transfers it to the DLQ.
    assert!(report_id.contains('-'), "report id exchanged: {report_id}");
    let claim1 = rf(
        &a,
        &[
            b"XCLAIM",
            b"jobs/todo",
            b"workers",
            b"w2",
            b"0",
            report_id.as_bytes(),
        ],
    )
    .await;
    assert!(
        claim1.contains("report-nightly"),
        "claim #2 still delivers: {claim1}"
    );
    let claim2 = rf(
        &a,
        &[
            b"XCLAIM",
            b"jobs/todo",
            b"workers",
            b"w3",
            b"0",
            report_id.as_bytes(),
        ],
    )
    .await;
    assert!(
        !claim2.contains("report-nightly"),
        "claim #3 transfers: {claim2}"
    );
    run(
        &mut c,
        "UPDATE job_ledger SET state = 'dead', tries = tries + 2 WHERE job = 'report-nightly'",
    )
    .await;

    // e) The DLQ carries the original payload plus the trace fields.
    let dlq = rf(&a, &[b"XRANGE", b"jobs/todo/dlq", b"-", b"+"]).await;
    assert!(
        dlq.contains("report-nightly") && dlq.contains("__dlq_group") && dlq.contains("__dlq_src"),
        "{dlq}"
    );
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"jobs/todo/dlq"]).await), 1);

    // f) Reconciliation: ledger states match the MQ reality.
    let state = one(
        &mut c,
        "SELECT state FROM job_ledger WHERE job = 'email-welcome'",
    )
    .await;
    assert!(
        matches!(&state, MVal::Bytes(b) if b == b"done"),
        "{state:?}"
    );
    let dead = one(
        &mut c,
        "SELECT state FROM job_ledger WHERE job = 'report-nightly'",
    )
    .await;
    assert!(matches!(&dead, MVal::Bytes(b) if b == b"dead"), "{dead:?}");
    let pending = one(&mut c, "SELECT state FROM job_ledger WHERE job = 'cleanup'").await;
    assert!(
        matches!(&pending, MVal::Bytes(b) if b == b"pending"),
        "{pending:?}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"jobs/todo"]).await),
        2,
        "cleanup still staged, email+report exchanged"
    );
    // The lease is still held inside its TTL.
    assert_eq!(rb(&a, &[b"GET", b"lock:job:email-welcome"]).await, "exec-1");
}
