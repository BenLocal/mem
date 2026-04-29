# Claude Code Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable invisible memory capture and recall for Claude Code users through hooks and CLI commands.

**Architecture:** Two CLI subcommands (`mem mine` for transcript parsing, `mem wake-up` for memory injection) + three bash hook scripts that call these commands in background.

**Tech Stack:** Rust (clap, serde_json, regex, reqwest), bash, jq

---

## File Structure

**New files:**
- `src/cli/mine.rs` - Transcript parser and memory extractor
- `src/cli/wake_up.rs` - Memory query and formatter for session start
- `hooks/claude_code_stop.sh` - Stop hook (every 15 exchanges)
- `hooks/claude_code_precompact.sh` - PreCompact hook
- `hooks/claude_code_sessionstart.sh` - SessionStart hook
- `tests/cli_mine.rs` - Integration tests for mine command
- `tests/cli_wake_up.rs` - Integration tests for wake-up command

**Modified files:**
- `src/main.rs` - Add Mine and WakeUp subcommands
- `src/cli/mod.rs` - Export mine and wake_up modules
- `Cargo.toml` - Add regex dependency
- `README.md` - Add installation instructions

---

### Task 1: Add regex dependency

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add regex to dependencies**

```toml
regex = "1"
```

Add after line 20 (after `serde_json`).

- [ ] **Step 2: Verify build**

Run: `cargo check`
Expected: SUCCESS

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add regex dependency for pattern matching"
```

---

### Task 2: Create transcript parser module

**Files:**
- Create: `src/cli/mine.rs`

- [ ] **Step 1: Write test for parsing Claude Code JSONL**

Create `tests/cli_mine.rs`:

```rust
use std::fs;
use tempfile::NamedTempFile;

#[test]
fn test_parse_claude_code_transcript() {
    let transcript = r#"{"type":"custom-title","sessionId":"abc"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Test memory</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    
    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "Test memory");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_parse_claude_code_transcript`
Expected: FAIL with "module not found"

- [ ] **Step 3: Create mine module skeleton**

Create `src/cli/mine.rs`:

```rust
use anyhow::Result;
use std::path::Path;

pub struct ExtractedMemory {
    pub content: String,
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
}

pub fn parse_transcript(path: &Path) -> Result<Vec<ExtractedMemory>> {
    Ok(vec![])
}
```

- [ ] **Step 4: Export mine module**

Modify `src/cli/mod.rs`:

```rust
pub mod mcp;
pub mod mine;
pub mod repair;
pub mod serve;
```

- [ ] **Step 5: Run test to verify it compiles but fails assertion**

Run: `cargo test test_parse_claude_code_transcript`
Expected: FAIL with "assertion failed: memories.len() == 1"

- [ ] **Step 6: Commit**

```bash
git add src/cli/mine.rs src/cli/mod.rs tests/cli_mine.rs
git commit -m "test(mine): add transcript parsing test"
```

### Task 3: Implement transcript parsing logic

**Files:**
- Modify: `src/cli/mine.rs`

- [ ] **Step 1: Implement JSONL parsing**

```rust
use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct ExtractedMemory {
    pub content: String,
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
}

pub fn parse_transcript(path: &Path) -> Result<Vec<ExtractedMemory>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();
    
    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        
        if value["type"] != "assistant" {
            continue;
        }
        
        let session_id = value["sessionId"].as_str().unwrap_or("").to_string();
        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();
        
        if let Some(content_array) = value["message"]["content"].as_array() {
            for item in content_array {
                if let Some(text) = item["text"].as_str() {
                    if let Some(extracted) = extract_memory(text) {
                        memories.push(ExtractedMemory {
                            content: extracted,
                            session_id: session_id.clone(),
                            timestamp: timestamp.clone(),
                            line_number: line_num + 1,
                        });
                    }
                }
            }
        }
    }
    
    Ok(memories)
}

fn extract_memory(text: &str) -> Option<String> {
    None
}
```

- [ ] **Step 2: Run test**

Run: `cargo test test_parse_claude_code_transcript`
Expected: FAIL (extract_memory returns None)

- [ ] **Step 3: Implement hybrid extraction (explicit tags + patterns)**

```rust
fn extract_memory(text: &str) -> Option<String> {
    // Priority 1: Explicit <mem-save> tags
    let tag_re = Regex::new(r"<mem-save>(.*?)</mem-save>").unwrap();
    if let Some(cap) = tag_re.captures(text) {
        return Some(cap[1].trim().to_string());
    }
    
    // Priority 2: Pattern matching
    let patterns = [
        r"(?:我会记住：|关键发现：|重要：)(.+?)(?:\n|$)",
        r"(?:I'll remember:|Key insight:|Important:)(.+?)(?:\n|$)",
    ];
    
    for pattern in &patterns {
        let re = Regex::new(pattern).unwrap();
        if let Some(cap) = re.captures(text) {
            return Some(cap[1].trim().to_string());
        }
    }
    
    None
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test test_parse_claude_code_transcript`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/cli/mine.rs
git commit -m "feat(mine): implement transcript parsing with hybrid extraction"
```

---

### Task 4: Add pattern matching tests

**Files:**
- Modify: `tests/cli_mine.rs`

- [ ] **Step 1: Write test for Chinese patterns**

```rust
#[test]
fn test_extract_chinese_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我会记住：这是重要信息"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    
    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "这是重要信息");
}
```

- [ ] **Step 2: Run test**

Run: `cargo test test_extract_chinese_pattern`
Expected: PASS

- [ ] **Step 3: Write test for English patterns**

```rust
#[test]
fn test_extract_english_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Key insight: This is important"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    
    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "This is important");
}
```

- [ ] **Step 4: Run test**

Run: `cargo test test_extract_english_pattern`
Expected: PASS

- [ ] **Step 5: Write test for tag priority**

```rust
#[test]
fn test_tag_priority_over_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll remember: wrong\n<mem-save>correct</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    
    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "correct");
}
```

- [ ] **Step 6: Run test**

Run: `cargo test test_tag_priority_over_pattern`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add tests/cli_mine.rs
git commit -m "test(mine): add pattern matching tests"
```

---

### Task 5: Implement mem mine CLI command

**Files:**
- Modify: `src/cli/mine.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add CLI args structure**

In `src/cli/mine.rs`:

```rust
use clap::Args;

#[derive(Debug, Args)]
pub struct MineArgs {
    /// Path to Claude Code transcript file
    pub transcript_path: std::path::PathBuf,
    
    /// Tenant ID
    #[arg(long, default_value = "local")]
    pub tenant: String,
    
    /// Source agent name
    #[arg(long, default_value = "claude-code")]
    pub agent: String,
    
    /// Base URL for mem service
    #[arg(long, env = "MEM_BASE_URL", default_value = "http://127.0.0.1:3000")]
    pub base_url: String,
}
```

- [ ] **Step 2: Implement run function**

```rust
pub async fn run(args: MineArgs) -> i32 {
    let memories = match parse_transcript(&args.transcript_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to parse transcript: {}", e);
            return 1;
        }
    };
    
    let client = reqwest::Client::new();
    let mut success = 0;
    let mut failed = 0;
    
    for memory in memories {
        let idempotency_key = format!("{}:{}", args.transcript_path.display(), memory.line_number);
        
        let payload = serde_json::json!({
            "tenant": args.tenant,
            "memory_type": "observation",
            "content": memory.content,
            "summary": memory.content.chars().take(80).collect::<String>(),
            "scope": "global",
            "source_agent": args.agent,
            "idempotency_key": idempotency_key,
            "write_mode": "auto",
        });
        
        match client.post(format!("{}/memories", args.base_url))
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status() == 409 => {
                success += 1;
            }
            Ok(resp) => {
                eprintln!("Failed to save memory: {}", resp.status());
                failed += 1;
            }
            Err(e) => {
                eprintln!("Request error: {}", e);
                failed += 1;
            }
        }
    }
    
    println!("Mined {} memories ({} success, {} failed)", success + failed, success, failed);
    if failed > 0 { 1 } else { 0 }
}
```

- [ ] **Step 3: Add Mine subcommand to main.rs**

```rust
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP memory service (default).
    Serve,
    /// Run the MCP (Model Context Protocol) stdio server.
    Mcp,
    /// Diagnose or rebuild the vector index sidecar.
    Repair(mem::cli::repair::RepairArgs),
    /// Mine memories from Claude Code transcript.
    Mine(mem::cli::mine::MineArgs),
}
```

- [ ] **Step 4: Add match arm**

```rust
match command {
    Command::Serve => mem::cli::serve::run().await,
    Command::Mcp => mem::cli::mcp::run().await,
    Command::Repair(args) => {
        let code = mem::cli::repair::run(args).await;
        std::process::exit(code);
    }
    Command::Mine(args) => {
        let code = mem::cli::mine::run(args).await;
        std::process::exit(code);
    }
}
```

- [ ] **Step 5: Test manually**

```bash
# Start mem serve in background
cargo run -- serve &

# Create test transcript
cat > /tmp/test.jsonl <<'EOF'
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Test memory from CLI</mem-save>"}]},"sessionId":"test","timestamp":"2026-04-29T10:00:00Z"}
EOF

# Run mine command
cargo run -- mine /tmp/test.jsonl

# Verify output
# Expected: "Mined 1 memories (1 success, 0 failed)"
```

- [ ] **Step 6: Commit**

```bash
git add src/cli/mine.rs src/main.rs
git commit -m "feat(mine): add mem mine CLI command"
```

---

### Task 6: Implement mem wake-up command

**Files:**
- Create: `src/cli/wake_up.rs`

- [ ] **Step 1: Write test for wake-up output**

Create `tests/cli_wake_up.rs`:

```rust
use std::fs;

#[tokio::test]
async fn test_wake_up_format() {
    // Start test server and seed memories
    let output = mem::cli::wake_up::run(mem::cli::wake_up::WakeUpArgs {
        tenant: "local".to_string(),
        token_budget: 800,
        base_url: "http://127.0.0.1:3000".to_string(),
    }).await.unwrap();
    
    assert!(output.contains("## Recent Context"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_wake_up_format`
Expected: FAIL with "module not found"

- [ ] **Step 3: Create wake_up module skeleton**

Create `src/cli/wake_up.rs`:

```rust
use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct WakeUpArgs {
    #[arg(long, default_value = "local")]
    pub tenant: String,
    
    #[arg(long, default_value = "800")]
    pub token_budget: usize,
    
    #[arg(long, env = "MEM_BASE_URL", default_value = "http://127.0.0.1:3000")]
    pub base_url: String,
}

pub async fn run(args: WakeUpArgs) -> Result<String> {
    Ok("## Recent Context\n".to_string())
}
```

- [ ] **Step 4: Export wake_up module**

Modify `src/cli/mod.rs`:

```rust
pub mod mcp;
pub mod mine;
pub mod repair;
pub mod serve;
pub mod wake_up;
```

- [ ] **Step 5: Run test**

Run: `cargo test test_wake_up_format`
Expected: PASS

- [ ] **Step 6: Implement wake-up logic**

```rust
pub async fn run(args: WakeUpArgs) -> Result<String> {
    let mut output = String::from("## Recent Context\n\n");
    
    // L0: Identity file
    if let Ok(identity) = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/.mem/identity.txt") {
        output.push_str(&identity);
        output.push_str("\n\n");
    }
    
    // L1: Query recent memories
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "tenant": args.tenant,
        "query": "",
        "limit": 10,
    });
    
    let resp = client.post(format!("{}/memories/search", args.base_url))
        .json(&payload)
        .send()
        .await?;
    
    if resp.status().is_success() {
        let data: serde_json::Value = resp.json().await?;
        if let Some(memories) = data["memories"].as_array() {
            for memory in memories {
                if let Some(content) = memory["content"].as_str() {
                    output.push_str("- ");
                    output.push_str(&content.chars().take(200).collect::<String>());
                    output.push_str("\n");
                }
            }
        }
    }
    
    Ok(output)
}
```

- [ ] **Step 7: Add WakeUp subcommand to main.rs**

```rust
#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    Mcp,
    Repair(mem::cli::repair::RepairArgs),
    Mine(mem::cli::mine::MineArgs),
    /// Query and format memories for session start injection.
    WakeUp(mem::cli::wake_up::WakeUpArgs),
}
```

- [ ] **Step 8: Add match arm**

```rust
Command::WakeUp(args) => {
    match mem::cli::wake_up::run(args).await {
        Ok(output) => {
            print!("{}", output);
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to wake up: {}", e);
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 9: Test manually**

```bash
# Ensure mem serve is running
cargo run -- serve &

# Run wake-up
cargo run -- wake-up

# Expected output:
# ## Recent Context
# 
# [memories listed]
```

- [ ] **Step 10: Commit**

```bash
git add src/cli/wake_up.rs src/cli/mod.rs src/main.rs tests/cli_wake_up.rs
git commit -m "feat(wake-up): add mem wake-up CLI command"
```

---

### Task 7: Create hook scripts

**Files:**
- Create: `hooks/claude_code_stop.sh`
- Create: `hooks/claude_code_precompact.sh`
- Create: `hooks/claude_code_sessionstart.sh`

- [ ] **Step 1: Create Stop hook**

Create `hooks/claude_code_stop.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty')

if [ -z "$TRANSCRIPT" ]; then
    echo '{}'
    exit 0
fi

EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT" 2>/dev/null || echo 0)
LAST_SAVE_FILE="$HOME/.mem/last_save"
LAST_SAVE=$(cat "$LAST_SAVE_FILE" 2>/dev/null || echo 0)

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -ge 15 ]; then
    mem mine "$TRANSCRIPT" --agent claude-code &
    echo "$EXCHANGE_COUNT" > "$LAST_SAVE_FILE"
fi

echo '{}'
```

- [ ] **Step 2: Make executable**

```bash
chmod +x hooks/claude_code_stop.sh
```

- [ ] **Step 3: Create PreCompact hook**

Create `hooks/claude_code_precompact.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath // empty')

if [ -n "$TRANSCRIPT" ]; then
    mem mine "$TRANSCRIPT" --agent claude-code &
fi

echo '{}'
```

- [ ] **Step 4: Make executable**

```bash
chmod +x hooks/claude_code_precompact.sh
```

- [ ] **Step 5: Create SessionStart hook**

Create `hooks/claude_code_sessionstart.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

WAKEUP=$(mem wake-up --tenant local --token-budget 800 2>/dev/null || echo "")

if [ -n "$WAKEUP" ]; then
    ESCAPED=$(echo "$WAKEUP" | jq -Rs .)
    cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": $ESCAPED
  }
}
EOF
else
    echo '{}'
fi
```

- [ ] **Step 6: Make executable**

```bash
chmod +x hooks/claude_code_sessionstart.sh
```

- [ ] **Step 7: Test Stop hook**

```bash
# Create test input
echo '{"transcriptPath":"/tmp/test.jsonl"}' | ./hooks/claude_code_stop.sh

# Expected: {} (empty JSON)
# Check: ~/.mem/last_save should be created
```

- [ ] **Step 8: Test SessionStart hook**

```bash
./hooks/claude_code_sessionstart.sh

# Expected: JSON with additionalContext field
```

- [ ] **Step 9: Commit**

```bash
git add hooks/
git commit -m "feat(hooks): add Claude Code hook scripts"
```

---

### Task 8: Add README documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add installation section**

Add after existing content:

```markdown
## Claude Code Integration

### Installation

1. **Install hooks**:

```bash
mkdir -p ~/.mem/hooks
cp hooks/claude_code_*.sh ~/.mem/hooks/
```

2. **Register hooks** in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": "~/.mem/hooks/claude_code_stop.sh",
    "PreCompact": "~/.mem/hooks/claude_code_precompact.sh",
    "SessionStart": "~/.mem/hooks/claude_code_sessionstart.sh"
  }
}
```

3. **(Optional) Create identity file**:

```bash
cat > ~/.mem/identity.txt <<EOF
I am a [role] working on [domain].
I prefer [preferences].
EOF
```

### Usage

Hooks run automatically:
- **Stop**: Every 15 exchanges, mines memories in background
- **PreCompact**: Before context compression, final mine
- **SessionStart**: Injects recent memories at session start

Manual commands:

```bash
# Mine a transcript
mem mine ~/.claude/projects/.../session.jsonl

# Get wake-up context
mem wake-up --token-budget 800
```
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add Claude Code integration instructions"
```

---

### Task 9: Integration test - end-to-end flow

**Files:**
- Create: `tests/integration_claude_code.rs`

- [ ] **Step 1: Write end-to-end test**

```rust
use std::fs;
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_mine_and_wake_up_flow() {
    // Start test server
    let config = mem::config::Config::from_env().unwrap();
    let app = mem::app::router_with_config(config.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    
    let base_url = format!("http://{}", addr);
    
    // Create test transcript
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Integration test memory</mem-save>"}]},"sessionId":"test","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    
    // Mine memories
    let mine_args = mem::cli::mine::MineArgs {
        transcript_path: file.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: base_url.clone(),
    };
    
    let exit_code = mem::cli::mine::run(mine_args).await;
    assert_eq!(exit_code, 0);
    
    // Wake up and verify
    let wake_args = mem::cli::wake_up::WakeUpArgs {
        tenant: "local".to_string(),
        token_budget: 800,
        base_url: base_url.clone(),
    };
    
    let output = mem::cli::wake_up::run(wake_args).await.unwrap();
    assert!(output.contains("Integration test memory"));
}
```

- [ ] **Step 2: Run test**

Run: `cargo test test_mine_and_wake_up_flow`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/integration_claude_code.rs
git commit -m "test(integration): add end-to-end Claude Code flow test"
```

---

### Task 10: Final verification and cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run full test suite**

```bash
cargo test
```

Expected: All tests PASS

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --all-targets -- -D warnings
```

Expected: No warnings

- [ ] **Step 3: Run fmt check**

```bash
cargo fmt --check
```

Expected: No formatting issues

- [ ] **Step 4: Manual smoke test**

```bash
# Start server
cargo run -- serve &

# Create real transcript
# (use actual Claude Code session file)

# Mine it
cargo run -- mine ~/.claude/projects/.../session.jsonl

# Wake up
cargo run -- wake-up

# Verify memories appear
```

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "feat(integration): Claude Code seamless integration (closes ROADMAP #12)"
```

---

## Self-Review Checklist

**Spec coverage:**
- ✅ Transcript parsing (Task 2-3)
- ✅ Hybrid extraction (Task 3-4)
- ✅ mem mine CLI (Task 5)
- ✅ mem wake-up CLI (Task 6)
- ✅ Hook scripts (Task 7)
- ✅ Idempotency (Task 5, line_number in key)
- ✅ Installation docs (Task 8)
- ✅ Integration tests (Task 9)

**Placeholder scan:**
- ✅ No TBD/TODO
- ✅ All code blocks complete
- ✅ All commands have expected output

**Type consistency:**
- ✅ ExtractedMemory struct used consistently
- ✅ MineArgs/WakeUpArgs match usage
- ✅ JSON payload fields match HTTP API

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-29-claude-code-integration.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?

