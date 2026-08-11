#![cfg(unix)]

use std::process::Command as StdCommand;

use gloop_core::{FinalStatus, Node, NodeKind, NodeStatus};
use gloop_provider::ProviderRegistry;
use gloop_runtime::RunOptions;
use tempfile::tempdir;
use tokio::fs;
use tokio::time::{Duration, sleep};

#[tokio::test]
async fn command_output_overflow_is_non_retryable_and_cleans_up() {
    let temp = tempdir().expect("tempdir");
    let pid_path = temp.path().join("command-pid.txt");
    let marker_path = temp.path().join("attempts.log");
    let pid_file = pid_path.to_string_lossy().to_string();
    let marker = marker_path.to_string_lossy().to_string();
    let script = format!(
        r#"
set -eu
if [ -f "{pid_file}" ]; then
  OLD_PID="$(cat "{pid_file}")"
  if kill -0 "$OLD_PID" 2>/dev/null; then
    echo "overlap:$OLD_PID:$$" >> "{marker}"
    exit 1
  fi
fi
echo "$$" > "{pid_file}"
echo "start:$$" >> "{marker}"
i=0
while :; do
  printf '1234567890'
  sleep 0.01
  i=$((i+1))
  if [ "$i" -ge 120 ]; then
    break
  fi
done
"#,
    );

    let mut node = Node::command(
        "overflow",
        vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
    );
    if let NodeKind::Command { output, .. } = &mut node.kind {
        output.max_bytes = 32;
    }
    node.retry.max_attempts = 2;
    node.retry.backoff_seconds = 0;

    let graph = gloop_core::Graph::new("command-cleanup", "command cleanup", vec![node]);
    let runtime =
        gloop_runtime::Runtime::new(ProviderRegistry::default(), temp.path().join("runs"));
    let summary = runtime
        .run(&graph, RunOptions::default())
        .await
        .expect("run command");

    let log = fs::read_to_string(&marker_path).await.unwrap_or_default();
    let start_count = log
        .lines()
        .filter(|line| line.starts_with("start:"))
        .count();
    let overlap_count = log
        .lines()
        .filter(|line| line.starts_with("overlap:"))
        .count();
    let pids = collect_pids(&log);
    let alive = pids
        .iter()
        .copied()
        .filter(|pid| is_pid_alive(*pid))
        .collect::<Vec<_>>();
    if !alive.is_empty() {
        kill_logged_processes(&alive);
    }
    let state_content = fs::read_to_string(&pid_file).await.unwrap_or_default();

    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(summary.nodes["overflow"].status, NodeStatus::Failed);
    assert_eq!(summary.nodes["overflow"].attempts, 1);
    assert_eq!(
        start_count, 1,
        "deterministic command output overflow must execute only once"
    );
    assert!(
        state_content
            .lines()
            .filter_map(|line| line.parse::<u32>().ok())
            .all(|pid| !is_pid_alive(pid)),
        "recorded pids must be non-live after retries complete",
    );
    assert_eq!(
        overlap_count, 0,
        "cleanup must not overlap command processes"
    );
}

#[tokio::test]
async fn descendant_held_pipe_is_bounded_after_direct_child_exits() {
    let temp = tempdir().expect("tempdir");
    let descendant_path = temp.path().join("descendant-pid.txt");
    let descendant_file = descendant_path.to_string_lossy();
    let script = format!("sleep 30 & echo $! > {descendant_file:?}; printf ready",);
    let mut node = Node::command(
        "held_pipe",
        vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
    );
    node.retry.max_attempts = 2;
    let graph = gloop_core::Graph::new("held-pipe", "held pipe", vec![node]);
    let runtime =
        gloop_runtime::Runtime::new(ProviderRegistry::default(), temp.path().join("runs"));

    let summary = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.run(&graph, RunOptions::default()),
    )
    .await
    .expect("pipe drain must be bounded")
    .expect("run command");

    let descendant_pid = fs::read_to_string(&descendant_path)
        .await
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    if let Some(pid) = descendant_pid {
        for _ in 0..12 {
            if !is_pid_alive(pid) {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !is_pid_alive(pid),
            "background descendant must be terminated by command cleanup"
        );
    }

    let outcome = &summary.nodes["held_pipe"];
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("output pipes remained open"))
    );
}

#[tokio::test]
async fn command_environment_only_uses_portable_and_declared_values() {
    let temp = tempdir().expect("temp");
    #[cfg(unix)]
    let mut node = Node::command(
        "env",
        vec![
            "/bin/sh".to_owned(),
            "-lc".to_owned(),
            "env | sort".to_owned(),
        ],
    );
    #[cfg(windows)]
    let mut node = Node::command(
        "env",
        vec!["cmd".to_owned(), "/C".to_owned(), "set".to_owned()],
    );
    if let NodeKind::Command { env, .. } = &mut node.kind {
        env.insert("GRAPH_CANARY".to_owned(), "enabled".to_owned());
    }

    let graph = gloop_core::Graph::new("env-canary", "env canary", vec![node]);
    let runtime =
        gloop_runtime::Runtime::new(ProviderRegistry::default(), temp.path().join("runs"));
    let summary = runtime
        .run(&graph, RunOptions::default())
        .await
        .expect("run command");
    let output = summary.nodes["env"]
        .output
        .as_ref()
        .expect("command output")
        .as_str()
        .expect("text output");

    assert_eq!(summary.nodes["env"].status, NodeStatus::Succeeded);
    assert!(output.contains("GRAPH_CANARY=enabled"));
    #[cfg(unix)]
    assert!(!output.contains("LD_LIBRARY_PATH"));
    #[cfg(windows)]
    assert!(!output.contains("ALLUSERSPROFILE"));
    assert!(output.contains("PATH="));
}

#[tokio::test]
async fn missing_command_executable_is_deterministic_and_non_retryable() {
    let temp = tempdir().expect("temp");
    let mut node = Node::command("missing", vec!["/definitely-missing-command".to_owned()]);
    node.retry.max_attempts = 3;
    node.retry.backoff_seconds = 0;

    let graph = gloop_core::Graph::new("missing-command", "missing command", vec![node]);
    let runtime =
        gloop_runtime::Runtime::new(ProviderRegistry::default(), temp.path().join("runs"));
    let summary = runtime
        .run(&graph, RunOptions::default())
        .await
        .expect("run command");

    let outcome = &summary.nodes["missing"];
    assert_eq!(summary.status, FinalStatus::Failed);
    assert_eq!(outcome.status, NodeStatus::Failed);
    assert_eq!(outcome.attempts, 1);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("failed to start command"))
    );
}

fn collect_pids(log: &str) -> Vec<u32> {
    log.lines()
        .flat_map(|line| line.split(':'))
        .filter_map(|piece| piece.parse::<u32>().ok())
        .collect()
}

fn kill_logged_processes(pids: &[u32]) {
    for pid in pids {
        let _ = StdCommand::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
}

fn is_pid_alive(pid: u32) -> bool {
    StdCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
