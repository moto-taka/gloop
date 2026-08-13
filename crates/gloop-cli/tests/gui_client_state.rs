use std::io;
use std::process::Command;

#[test]
fn gui_client_state_js_tests() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = match Command::new("node")
        .args(["--test", "tests/gui_client_state.test.mjs"])
        .current_dir(manifest_dir)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            panic!("GUI client JS tests require the node executable: {error}");
        }
        Err(error) => panic!("spawn node test runner: {error}"),
    };
    assert!(
        output.status.success(),
        "node tests failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
