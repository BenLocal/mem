use std::fs;
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_mine_and_wake_up_flow() {
    let config = mem::config::Config::from_env().unwrap();
    let app = mem::app::router_with_config(config.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let base_url = format!("http://{}", addr);

    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Integration test memory</mem-save>"}]},"sessionId":"test","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let mine_args = mem::cli::mine::MineArgs {
        transcript_path: file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: base_url.clone(),
    };

    let exit_code = mem::cli::mine::run(mine_args).await;
    assert_eq!(exit_code, 0);

    // Give embedding worker time to process
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let wake_args = mem::cli::wake_up::WakeUpArgs {
        tenant: "local".to_string(),
        token_budget: 800,
        base_url: base_url.clone(),
    };

    let output = mem::cli::wake_up::run(wake_args).await.unwrap();
    assert!(output.contains("Integration test memory"));
}
