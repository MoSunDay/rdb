//! Multi-store scenario #99 -- an omnichannel support desk on the SQL
//! plane: the same agents work email and chat tickets, and the compound
//! operators answer roster questions. UNION dedups the merged roster,
//! UNION ALL keeps per-channel duplicates, INTERSECT [ALL] finds the
//! double-hatted agents (min multiplicities for ALL), EXCEPT [ALL]
//! isolates email-only staffing (multiset subtraction for ALL),
//! INTERSECT binds tighter than UNION, and a trailing ORDER BY +
//! LIMIT pages the whole compound. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply.iter().map(|r| r.iter().map(cell).collect()).collect()
}

#[tokio::test]
async fn omnichannel_desk_unions_intersects_and_excepts_the_roster() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("omni99", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Two channels; Ada filed two email tickets (the duplicate the
    //    ALL/dedup contrast below turns on).
    ddl(&mut c, "CREATE TABLE email_tickets (id INT PRIMARY KEY, agent VARCHAR(16) NOT NULL, minutes INT NOT NULL)").await;
    ddl(&mut c, "CREATE TABLE chat_tickets (id INT PRIMARY KEY, agent VARCHAR(16) NOT NULL, minutes INT NOT NULL)").await;
    run(
        &mut c,
        "INSERT INTO email_tickets (id, agent, minutes) VALUES \
         (1, 'Ada', 30), (2, 'Grace', 10), (3, 'Ada', 45)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO chat_tickets (id, agent, minutes) VALUES \
         (4, 'Grace', 20), (5, 'Edsger', 15)",
    )
    .await;

    // b) UNION dedups across channels.
    let roster = rows(
        &mut c,
        "SELECT agent FROM email_tickets UNION SELECT agent FROM chat_tickets ORDER BY agent",
    )
    .await;
    assert_eq!(
        grid(&roster),
        vec![vec!["Ada"], vec!["Edsger"], vec!["Grace"]],
        "union dedups"
    );

    // c) UNION ALL keeps every row and admits literals in an operand.
    let ledger = rows(
        &mut c,
        "SELECT agent FROM email_tickets UNION ALL SELECT agent FROM chat_tickets \
         UNION ALL SELECT 'overflow' ORDER BY agent",
    )
    .await;
    assert_eq!(
        grid(&ledger),
        vec![
            vec!["Ada"],
            vec!["Ada"],
            vec!["Edsger"],
            vec!["Grace"],
            vec!["Grace"],
            vec!["overflow"]
        ],
        "union all keeps duplicates"
    );

    // d) INTERSECT: agents staffed on BOTH channels (set); INTERSECT
    //    ALL: min multiplicity per agent (Grace 1x email, 1x chat).
    let both = rows(
        &mut c,
        "SELECT agent FROM email_tickets INTERSECT SELECT agent FROM chat_tickets",
    )
    .await;
    assert_eq!(grid(&both), vec![vec!["Grace"]], "intersect both channels");
    let both_all = rows(
        &mut c,
        "SELECT agent FROM email_tickets INTERSECT ALL SELECT agent FROM chat_tickets",
    )
    .await;
    assert_eq!(
        grid(&both_all),
        vec![vec!["Grace"]],
        "intersect all = min multiplicity"
    );

    // e) EXCEPT: email-only (set); EXCEPT ALL: multiset subtraction
    //    (Ada 2x stays 2x, Grace 1x cancels).
    let email_only = rows(
        &mut c,
        "SELECT agent FROM email_tickets EXCEPT SELECT agent FROM chat_tickets",
    )
    .await;
    assert_eq!(grid(&email_only), vec![vec!["Ada"]], "except email-only");
    let email_only_all = rows(
        &mut c,
        "SELECT agent FROM email_tickets EXCEPT ALL SELECT agent FROM chat_tickets ORDER BY agent",
    )
    .await;
    assert_eq!(
        grid(&email_only_all),
        vec![vec!["Ada"], vec!["Ada"]],
        "except all subtracts"
    );

    // f) Precedence: INTERSECT binds tighter, so this reads
    //    email UNION (chat INTERSECT chat) = the union roster.
    let prec = rows(
        &mut c,
        "SELECT agent FROM email_tickets UNION SELECT agent FROM chat_tickets \
         INTERSECT SELECT agent FROM chat_tickets ORDER BY agent",
    )
    .await;
    assert_eq!(
        grid(&prec),
        vec![vec!["Ada"], vec!["Edsger"], vec!["Grace"]],
        "intersect binds tighter"
    );

    // g) A trailing ORDER BY + LIMIT pages the WHOLE compound (top-3
    //    handle time across both channels, Grace's 20m beats Ada's 10m).
    let board = rows(
        &mut c,
        "SELECT agent, minutes FROM email_tickets UNION ALL SELECT agent, minutes FROM chat_tickets \
         ORDER BY minutes DESC LIMIT 3",
    )
    .await;
    assert_eq!(
        grid(&board),
        vec![vec!["Ada", "45"], vec!["Ada", "30"], vec!["Grace", "20"]],
        "compound order by + limit"
    );
}
