//! `rdb-bench`: dependency-light RESP load generator for rdb.
//!
//! One client = one tokio task = one TCP connection. Each connection sends
//! `AUTH <token>` first, then loops until a shared deadline: build a batch
//! of `pipeline` RESP command frames, record the send timestamp, write the
//! batch, read every reply, and record ONE latency sample per batch. With
//! `pipeline=1` a batch is a single command, so the sample is the per-op
//! RTT; with larger pipelines every reported stat is a per-batch RTT.
//!
//! Modules: `cli` (argument parsing), `resp` (client-side RESP codec),
//! `client` (the per-connection load loop), `stats` (pure aggregation)
//! and `kafka`/`kafka_wire`/`kafka_reply` (the hand-rolled Produce v2 /
//! Fetch v4 client for the kafka front).

mod cli;
mod client;
mod kafka;
mod kafka_reply;
mod kafka_wire;
mod resp;
mod stats;

use std::time::Duration;
use std::time::Instant;

use cli::parse_args;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_args(&args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("{}", cli::usage());
            std::process::exit(2);
        }
    };

    // Kafka workloads need the topic partition to exist before any
    // client connects (the kafka front never auto-creates topics), so
    // seed `<topic>/q0` over RESP once, up front.
    if cfg.workload.is_kafka() {
        if let Err(msg) = kafka::ensure_topic(&cfg).await {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    }

    // Shared deadline: tasks stop issuing batches after it and only drain
    // the replies of the batch already in flight.
    let deadline = Instant::now() + Duration::from_secs(cfg.duration);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(cfg.clients);
    for client_id in 0..cfg.clients {
        let task_cfg = cfg.clone();
        handles.push(tokio::spawn(async move {
            if task_cfg.workload.is_kafka() {
                kafka::run_kafka_client(&task_cfg, client_id, deadline).await
            } else {
                client::run_client(&task_cfg, client_id, deadline).await
            }
        }));
    }

    let mut all: Vec<client::ClientStats> = Vec::with_capacity(cfg.clients);
    let mut fatal: Option<String> = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(stats)) => all.push(stats),
            Ok(Err(msg)) => {
                if fatal.is_none() {
                    fatal = Some(msg);
                }
            }
            Err(join) => {
                if fatal.is_none() {
                    fatal = Some(format!("client task failed: {join}"));
                }
            }
        }
    }

    let code = stats::report(&cfg, started.elapsed().as_secs_f64(), &all);
    if let Some(msg) = fatal {
        eprintln!("error: {msg}");
        std::process::exit(1);
    }
    std::process::exit(code);
}
