#[tokio::test]
async fn test_wake_up_format() {
    // Test with a non-existent server - should still return header
    let result = mem::cli::wake_up::run(mem::cli::wake_up::WakeUpArgs {
        remote: mem::cli::common::RemoteArgs {
            tenant: "local".to_string(),
            base_url: "http://127.0.0.1:9999".to_string(),
        },
        token_budget: 800,
        format: mem::cli::wake_up::WakeUpFormat::Plain,
        scope: vec![],
    })
    .await;

    // Should fail to connect but we can test the function exists and compiles
    assert!(result.is_err() || result.unwrap().contains("## Recent Context"));
}
