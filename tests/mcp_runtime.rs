#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn mcp_runtime_does_not_scale_threads_with_cpu_count() {
    let child = Command::new(env!("CARGO_BIN_EXE_mem"))
        .arg("mcp")
        // Force the old CPU-scaled runtime to expose the regression even on a
        // small CI runner. The MCP runtime's explicit worker count wins over
        // this setting.
        .env("TOKIO_WORKER_THREADS", "32")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mem mcp");
    let mut child = ChildGuard(child);
    let task_dir = format!("/proc/{}/task", child.0.id());
    let startup_deadline = Instant::now() + Duration::from_secs(10);
    let mut max_threads = 0;
    while max_threads < 9 && Instant::now() < startup_deadline {
        assert_eq!(child.0.try_wait().expect("poll mem mcp"), None);
        let count = std::fs::read_dir(&task_dir)
            .expect("read mem mcp task directory")
            .count();
        max_threads = max_threads.max(count);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        max_threads >= 9,
        "mem mcp runtime did not start eight async workers within ten seconds; got {max_threads} threads"
    );

    let sampling_started = Instant::now();
    while sampling_started.elapsed() < Duration::from_secs(1) {
        assert_eq!(child.0.try_wait().expect("poll mem mcp"), None);
        let count = std::fs::read_dir(&task_dir)
            .expect("read mem mcp task directory")
            .count();
        max_threads = max_threads.max(count);
        thread::sleep(Duration::from_millis(20));
    }

    assert!(
        (9..=16).contains(&max_threads),
        "mem mcp should use a main thread, eight async workers, and only a few support threads; got {max_threads}"
    );
}
