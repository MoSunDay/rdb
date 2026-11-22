//! Multi-store scenario #79 -- a print spooler on the LPOP/RPOP count
//! forms: a positive count answers an ARRAY in pop order (head-first
//! for LPOP, tail-first for RPOP), a count larger than the queue takes
//! what is there (short take, no error), count 0 answers *0, a missing
//! key answers the counted *0 vs the single-element $-1, negative and
//! non-integer counts refuse verbatim, and both ends drain
//! symmetrically. One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, int_of, r, rf, rt, FrontSpec};

/// Sorted member list of a set/array reply (server order checked
/// explicitly where it matters, so the audit may sort for equality).
fn bulks(reply: &str) -> Vec<String> {
    let n: usize = reply
        .split("\r\n")
        .next()
        .unwrap_or("")
        .trim_start_matches('*')
        .parse()
        .unwrap_or(0);
    (0..n)
        .map(|i| scenario_common::bulk_at(reply.as_bytes(), i))
        .collect()
}

#[tokio::test]
async fn spooler_pops_counted_from_both_ends() {
    let (_front, a) = front_world("spool79", &FrontSpec::default()).await;

    // a) Seed the spooler: head = job-a .. tail = job-d.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{sp}q", b"job-a", b"job-b", b"job-c", b"job-d"]).await), 4);

    // b) LPOP count 2: an ARRAY, head-first (oldest job leads).
    let got = rf(&a, &[b"LPOP", b"{sp}q", b"2"]).await;
    assert_eq!(bulks(&got), vec!["job-a", "job-b"], "head-first: {got}");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{sp}q"]).await), 2);

    // c) Short take: a count larger than the queue drains what is
    //    there without error.
    let got = rf(&a, &[b"LPOP", b"{sp}q", b"10"]).await;
    assert_eq!(bulks(&got), vec!["job-c", "job-d"], "short take: {got}");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{sp}q"]).await), 0, "drained key is gone");

    // d) RPOP count: tail-first (newest job leads the array).
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{sp}q", b"x", b"y", b"z"]).await), 3);
    let got = rf(&a, &[b"RPOP", b"{sp}q", b"2"]).await;
    assert_eq!(bulks(&got), vec!["z", "y"], "tail-first: {got}");
    // A count of 1 still answers the ARRAY form (not the bare bulk).
    let got = rf(&a, &[b"RPOP", b"{sp}q", b"1"]).await;
    assert!(got.starts_with("*1\r\n") && got.contains("x"), "counted single: {got}");

    // e) Missing key: counted form *0, single form $-1.
    assert_eq!(rf(&a, &[b"LPOP", b"{sp}gone", b"3"]).await, "*0\r\n");
    assert_eq!(rt(&a, &[b"LPOP", b"{sp}gone"]).await, "$-1");
    assert_eq!(rt(&a, &[b"RPOP", b"{sp}gone"]).await, "$-1");

    // f) count 0: a present, non-empty queue still answers *0 and
    //    keeps its jobs.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{sp}q", b"m", b"n"]).await), 2);
    assert_eq!(rf(&a, &[b"LPOP", b"{sp}q", b"0"]).await, "*0\r\n", "zero take");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{sp}q"]).await), 2, "nothing popped");

    // g) Negative and non-integer counts refuse verbatim.
    assert_eq!(
        rt(&a, &[b"LPOP", b"{sp}q", b"-1"]).await,
        "-ERR value is out of range, must be positive"
    );
    assert_eq!(
        rt(&a, &[b"RPOP", b"{sp}q", b"-3"]).await,
        "-ERR value is out of range, must be positive"
    );
    assert_eq!(
        rt(&a, &[b"LPOP", b"{sp}q", b"two"]).await,
        "-ERR value is not an integer or out of range"
    );
    assert_eq!(
        rt(&a, &[b"RPOP", b"{sp}q", b"2.5"]).await,
        "-ERR value is not an integer or out of range"
    );

    // h) A non-list squatting the name refuses both ends.
    assert_eq!(r(&a, &[b"SET", b"{sp}str", b"v"]).await, b"+OK");
    assert!(rt(&a, &[b"LPOP", b"{sp}str", b"1"]).await.starts_with("-WRONGTYPE"));
    assert!(rt(&a, &[b"RPOP", b"{sp}str", b"1"]).await.starts_with("-WRONGTYPE"));
}
