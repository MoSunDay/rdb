//! Per-test process fixture: spawns the REAL rdb binary with `s3_bind`
//! enabled and waits for its listeners.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

pub(super) const TOKEN: &str = "s3-e2e-fake-token-0123456789abcdef0123456789abcdef";

pub(super) struct Node {
    pub(super) child: Child,
    pub(super) dir: PathBuf,
    pub(super) stderr_path: PathBuf,
    pub(super) bind: String, // node name in checkpoint keys == conf.bind
    pub(super) http: String, // s3_bind
    pub(super) s3root: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(super) fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    l.local_addr().expect("local_addr").to_string()
}

pub(super) fn spawn_once(dir: &str, s3_token: &str) -> Node {
    // (Re)create the node dir: a failed spawn's `Node::drop` removes it,
    // and the retry loop below comes straight back here.
    std::fs::create_dir_all(dir).expect("create node dir");
    let (bind, raft, raft_http, monitor, http) =
        (free_addr(), free_addr(), free_addr(), free_addr(), free_addr());
    let s3root = PathBuf::from(dir).join("s3data");
    let yaml = format!(
        "bind: \"{bind}\"\nstore_path: \"{dir}\"\nraft_bind_address: \"{raft}\"\n\
         raft_http_bind_address: \"{raft_http}\"\nmonitor_addr: \"{monitor}\"\n\
         raft_token: \"{TOKEN}\"\ns3_bind: \"{http}\"\ns3_store_path: \"{}\"\n\
         s3_bucket: \"rdb\"\ns3_token: \"{s3_token}\"\ns3_checkpoint_interval_ms: 500\n",
        s3root.display(),
    );
    let config_path = PathBuf::from(dir).join("conf.yaml");
    std::fs::write(&config_path, yaml).expect("write conf.yaml");
    let stderr_path = PathBuf::from(dir).join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
    let child = Command::new(env!("CARGO_BIN_EXE_rdb"))
        .arg("-config")
        .arg(&config_path)
        .env("RAFT_BOOTSTRAP", "true")
        .env_remove("RAFT_JOIN_ADDR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn rdb binary");
    Node { child, dir: PathBuf::from(dir), stderr_path, bind, http, s3root }
}

pub(super) fn spawn_s3_node(dir: &str) -> Node {
    spawn_s3_node_retry(dir, TOKEN)
}

/// Same, with a caller-chosen `s3_token` (auth-edge-case tests).
pub(super) fn spawn_s3_node_with_token(dir: &str, s3_token: &str) -> Node {
    spawn_s3_node_retry(dir, s3_token)
}

fn spawn_s3_node_retry(dir: &str, s3_token: &str) -> Node {
    // A freed probe port can be grabbed between free_addr() and the child's
    // bind(); retry with a fresh address set when the binary exits early.
    for _ in 0..3 {
        let mut node = spawn_once(dir, s3_token);
        std::thread::sleep(Duration::from_millis(250));
        match node.child.try_wait() {
            Ok(Some(_)) => continue,
            _ => return node,
        }
    }
    let mut node = spawn_once(dir, s3_token);
    node.child.wait().expect("rdb run");
    panic!("rdb kept failing to bind; see {}", node.stderr_path.display());
}

pub(super) async fn wait_accepting(node: &mut Node, addr: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = node.child.try_wait() {
            let tail = std::fs::read_to_string(&node.stderr_path).unwrap_or_default();
            panic!("rdb exited before {what} ready (status {status})\nstderr:\n{tail}");
        }
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "rdb {what} never accepted on {addr}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn dir_for(tag: &str) -> String {
    let dir = std::env::temp_dir()
        .join(format!("rdb-s3-e2e-{tag}-{}", std::process::id()));
    dir.to_string_lossy().into_owned()
}
