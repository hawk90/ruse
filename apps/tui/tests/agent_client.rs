//! F-017 slice 1 integration test: the client↔agent split PROVEN end-to-end against the real `ruse agent`
//! subprocess (no SSH) — spawn it, handshake (version + capability negotiation with degradation), and read a
//! file THROUGH the agent (execution on the agent side, driven by the local client).

use ruse_tui::remote::client::AgentClient;
use std::process::Command;

#[test]
fn client_handshakes_and_reads_a_file_over_the_agent() {
    let path = std::env::temp_dir().join(format!("ruse_agent_it_{}.txt", std::process::id()));
    std::fs::write(&path, b"remote bytes").unwrap();

    // Spawn the real agent binary (`ruse agent`). The client WANTS more than the agent offers, so the
    // extra capability must degrade away rather than fail the connection.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ruse"));
    cmd.arg("agent");
    let mut client = AgentClient::spawn(cmd, &["fs.readFile", "search"]).expect("spawn agent");

    assert_eq!(
        client.protocol_version(),
        1,
        "handshake exchanges the protocol version"
    );
    assert!(
        client.has("fs.readFile"),
        "the offered service is negotiated in"
    );
    assert!(
        !client.has("search"),
        "a wanted-but-unoffered capability degrades, not errors"
    );

    let content = client.read_file(path.to_str().unwrap()).unwrap();
    assert_eq!(
        content, "remote bytes",
        "the client reads a file through the agent"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn client_writes_stats_and_lists_through_the_agent() {
    let dir = std::env::temp_dir().join(format!("ruse_agent_fs_it_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ruse"));
    cmd.arg("agent");
    let mut client =
        AgentClient::spawn(cmd, &["fs.readFile", "fs.writeFile", "fs.stat", "fs.list"])
            .expect("spawn agent");

    // Write a file THROUGH the agent, then read it back through the agent — the write is proven by the read.
    let file = dir.join("hello.txt");
    let fp = file.to_str().unwrap();
    let n = client.write_file(fp, "written remotely").unwrap();
    assert_eq!(n, "written remotely".len() as u64, "byte count echoed back");
    assert_eq!(
        client.read_file(fp).unwrap(),
        "written remotely",
        "the file the agent wrote reads back through the agent"
    );

    // stat: the written file exists; a missing sibling is `exists: false`, not an error.
    let st = client.stat(fp).unwrap();
    assert!(st.exists && st.is_file && st.len == 16);
    assert!(
        !client
            .stat(dir.join("ghost").to_str().unwrap())
            .unwrap()
            .exists,
        "a missing path stats as absent, not an error"
    );

    // list: the written file appears among the directory entries.
    let names: Vec<String> = client
        .list(dir.to_str().unwrap())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));

    let _ = std::fs::remove_dir_all(&dir);
}
