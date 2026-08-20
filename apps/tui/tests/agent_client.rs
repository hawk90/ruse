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
