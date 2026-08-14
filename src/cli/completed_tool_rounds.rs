use clap::{Args, Subcommand};
use serde::Serialize;

use super::common::RemoteArgs;
use crate::service::CompletedToolRoundRebuildReport;

#[derive(Debug, Subcommand)]
pub enum TranscriptRoundsCommand {
    /// Rebuild one session's transcript-derived completed-tool-round index.
    Rebuild(RebuildArgs),
}

#[derive(Debug, Args)]
pub struct RebuildArgs {
    #[command(flatten)]
    pub remote: RemoteArgs,

    /// Transcript session to rebuild. Required to keep one request bounded.
    #[arg(long)]
    pub session: String,

    /// Project and report without publishing a new generation.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Serialize)]
struct RebuildRequest<'a> {
    tenant: &'a str,
    session_id: &'a str,
    dry_run: bool,
}

pub async fn run(command: TranscriptRoundsCommand) -> i32 {
    match command {
        TranscriptRoundsCommand::Rebuild(args) => rebuild(args).await,
    }
}

async fn rebuild(args: RebuildArgs) -> i32 {
    let admin_token = match std::env::var("MEM_ADMIN_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token,
        _ => {
            eprintln!("MEM_ADMIN_TOKEN is required for transcript-rounds admin requests");
            return 1;
        }
    };
    let url = format!(
        "{}/admin/transcript-rounds/rebuild",
        args.remote.base_url.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(admin_token)
        .json(&RebuildRequest {
            tenant: &args.remote.tenant,
            session_id: &args.session,
            dry_run: args.dry_run,
        })
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            match response.json::<CompletedToolRoundRebuildReport>().await {
                Ok(body) => {
                    match serde_json::to_string_pretty(&body) {
                        Ok(rendered) => println!("{rendered}"),
                        Err(error) => {
                            eprintln!("render rebuild response: {error}");
                            return 1;
                        }
                    }
                    rebuild_exit_code(&body.status, body.degraded)
                }
                Err(error) => {
                    eprintln!("decode rebuild response: {error}");
                    1
                }
            }
        }
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            eprintln!("rebuild failed: HTTP {status}: {body}");
            1
        }
        Err(error) => {
            eprintln!("rebuild request failed: {error}");
            1
        }
    }
}

fn rebuild_exit_code(status: &str, degraded: bool) -> i32 {
    if !degraded && matches!(status, "published" | "unchanged" | "dry_run") {
        0
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::rebuild_exit_code;

    #[test]
    fn degraded_rebuild_is_not_reported_as_cli_success() {
        assert_eq!(rebuild_exit_code("published", false), 0);
        assert_eq!(rebuild_exit_code("unchanged", false), 0);
        assert_eq!(rebuild_exit_code("degraded", true), 2);
        assert_eq!(rebuild_exit_code("degraded", false), 2);
        assert_eq!(rebuild_exit_code("unexpected", false), 2);
    }
}
