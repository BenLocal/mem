# Claude Code Integration Package Design

**Date**: 2026-04-29  
**Status**: Approved  
**Roadmap**: Closes mempalace-diff §8 #13 (partial - Claude Code only)

---

## Overview

Seamless integration package for Claude Code that enables "invisible" memory capture and recall. Users work naturally with Claude, and memories are automatically saved and injected without explicit commands.

## Goals

1. **Invisible capture**: Memories saved in background without blocking AI responses
2. **Automatic recall**: Session start injects relevant memories automatically
3. **Hybrid extraction**: Explicit tags + pattern matching for robustness
4. **Idempotent**: Same transcript can be mined multiple times safely

## Non-Goals

- Codex integration (deferred to future PR)
- Real-time streaming mine (batch is sufficient)
- Capturing user messages (only assistant memories)

---

## Architecture

### Components

```
hooks/
├── claude_code_stop.sh          # Every N exchanges, trigger background mine
├── claude_code_precompact.sh    # Before context compression, final mine
└── claude_code_sessionstart.sh  # Inject wake-up memories at session start

src/cli/
├── mine.rs                      # Parse transcript, extract memories
└── wake_up.rs                   # Query and format memories for injection
```

### Data Flow

```
User ↔ Claude Code
         ↓ (every 15 exchanges)
    Stop Hook → mem mine (background) → POST /memories
         ↓ (session start)
    SessionStart Hook → mem wake-up → additionalContext
```

---

## Component Design

### 1. `mem mine` CLI

**Command**:
```bash
mem mine <transcript-path> [--tenant local] [--agent claude-code]
```

**Input**: Claude Code JSONL transcript
```json
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"..."}]},"sessionId":"...","timestamp":"..."}
```

**Extraction Strategy** (Hybrid):

1. **Explicit tags** (priority):
   ```
   <mem-save>This is important</mem-save>
   ```

2. **Pattern matching** (fallback):
   - Chinese: "我会记住："、"关键发现："、"重要："
   - English: "I'll remember:", "Key insight:", "Important:"

**Output**: POST to `/memories` with:
- `content`: Extracted text (verbatim)
- `summary`: First 80 chars
- `source_agent`: "claude-code"
- `session_id`: From transcript
- `idempotency_key`: `{transcript_path}:{line_number}`
- `memory_type`: "observation"
- `status`: "active"

**Idempotency**: Same transcript line never creates duplicate memories.

---

### 2. `mem wake-up` CLI

**Command**:
```bash
mem wake-up [--tenant local] [--token-budget 800]
```

**Output Format** (for SessionStart hook):
```
## Recent Context

[L0: Identity from ~/.mem/identity.txt if exists]

[L1: Top memories from recent sessions, ~700 tokens]
- Key fact 1
- Key fact 2
...
```

**Query Strategy**:
- Search with empty query (get recent active memories)
- Filter by last session_id (if available)
- Compress to token budget using existing compress logic

---

### 3. Hook Scripts

#### Stop Hook (`claude_code_stop.sh`)

**Trigger**: Every 15 user exchanges

**Logic**:
```bash
#!/usr/bin/env bash
INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath')
SESSION_ID=$(echo "$INPUT" | jq -r '.sessionId')

# Count exchanges since last save
EXCHANGE_COUNT=$(grep -c '"type":"user"' "$TRANSCRIPT")
LAST_SAVE=${LAST_SAVE:-0}

if [ $((EXCHANGE_COUNT - LAST_SAVE)) -ge 15 ]; then
    mem mine "$TRANSCRIPT" --agent claude-code &
    echo "$EXCHANGE_COUNT" > ~/.mem/last_save
fi

echo '{}'  # Empty JSON = natural stop
```

#### PreCompact Hook (`claude_code_precompact.sh`)

**Trigger**: Before context compression

**Logic**:
```bash
#!/usr/bin/env bash
INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | jq -r '.transcriptPath')

mem mine "$TRANSCRIPT" --agent claude-code &
echo '{}'
```

#### SessionStart Hook (`claude_code_sessionstart.sh`)

**Trigger**: Session start

**Logic**:
```bash
#!/usr/bin/env bash
WAKEUP=$(mem wake-up --tenant local --token-budget 800)

cat <<EOF
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "$WAKEUP"
  }
}
EOF
```

---

## Implementation Details

### Transcript Parsing

**Claude Code format**:
- Each line is a JSON object
- `type`: "user" | "assistant" | "custom-title" | "agent-name"
- `message.content[].text`: Actual message text
- `sessionId`: Session identifier
- `timestamp`: ISO8601 timestamp

**Parsing logic**:
1. Read line-by-line (JSONL)
2. Skip non-message types
3. Only process `type: "assistant"`
4. Extract text from `message.content[].text`
5. Apply hybrid extraction strategy

### Pattern Matching

**Regex patterns**:
```rust
// Explicit tags
<mem-save>(.*?)</mem-save>

// Chinese patterns
(我会记住：|关键发现：|重要：)(.+?)(?:\n|$)

// English patterns
(I'll remember:|Key insight:|Important:)(.+?)(?:\n|$)
```

### Idempotency

**Key format**: `{transcript_path}:{line_number}`

Example: `/Users/.../.claude/projects/.../abc123.jsonl:42`

**Behavior**:
- First mine: Creates memory
- Subsequent mines: 409 Conflict (ignored)
- Guarantees: No duplicate memories from same transcript line

---

## Installation

### 1. Hook Registration

Add to `~/.claude/settings.json`:
```json
{
  "hooks": {
    "Stop": "~/.mem/hooks/claude_code_stop.sh",
    "PreCompact": "~/.mem/hooks/claude_code_precompact.sh",
    "SessionStart": "~/.mem/hooks/claude_code_sessionstart.sh"
  }
}
```

### 2. Identity File (Optional)

Create `~/.mem/identity.txt`:
```
I am a senior Rust engineer working on distributed systems.
I prefer functional patterns and explicit error handling.
```

---

## Testing Strategy

### Unit Tests

1. **Transcript parsing**:
   - Parse valid Claude Code JSONL
   - Handle malformed lines gracefully
   - Extract sessionId and timestamp

2. **Pattern extraction**:
   - Explicit `<mem-save>` tags
   - Chinese patterns
   - English patterns
   - Mixed content

3. **Idempotency**:
   - Same line → same key
   - Different lines → different keys

### Integration Tests

1. **End-to-end mine**:
   - Create test transcript
   - Run `mem mine`
   - Verify POST to `/memories`
   - Re-run → no duplicates

2. **Wake-up**:
   - Seed memories
   - Run `mem wake-up`
   - Verify output format
   - Check token budget

### Hook Tests

1. **Stop hook**:
   - Mock transcript with 15+ exchanges
   - Verify background mine triggered
   - Check last_save file updated

2. **SessionStart hook**:
   - Run hook
   - Verify JSON output format
   - Check additionalContext populated

---

## Dependencies

### Rust Crates

- `serde_json`: JSONL parsing
- `regex`: Pattern matching
- `reqwest`: HTTP client (for POST /memories)
- Existing: `anyhow`, `clap`, `tokio`

### External Tools (Hooks)

- `bash`: Shell scripts
- `jq`: JSON parsing in hooks
- `grep`: Line counting

---

## Risks & Mitigations

### Risk 1: Transcript Format Changes

**Impact**: Parser breaks on Claude Code updates

**Mitigation**:
- Defensive parsing (skip unknown fields)
- Log warnings for unrecognized formats
- Graceful degradation (skip malformed lines)

### Risk 2: Pattern False Positives

**Impact**: Unintended content saved as memories

**Mitigation**:
- Explicit tags have priority
- Patterns require specific prefixes
- User can review via `/memories` endpoint

### Risk 3: Hook Performance

**Impact**: Background mine slows down session

**Mitigation**:
- Fork to background (`&`)
- Hook returns immediately
- Mine runs async, doesn't block AI

### Risk 4: Session Dependency

**Impact**: wake-up needs session_id from #11

**Mitigation**:
- Fallback to recent memories if no session
- Graceful degradation (no session filtering)
- Document dependency in README

---

## Future Work

1. **Codex integration**: Adapt for Codex transcript format
2. **Projects mode**: `mem mine --mode projects` for code scanning
3. **Configurable patterns**: User-defined extraction patterns
4. **Rich metadata**: Extract code_refs, tags from context

---

## Success Criteria

1. ✅ User writes message → AI responds → memory saved (invisible)
2. ✅ New session starts → relevant memories injected automatically
3. ✅ Same transcript mined twice → no duplicate memories
4. ✅ Hooks don't block AI responses (< 100ms overhead)
5. ✅ Works across macOS/Linux (bash compatibility)

---

## References

- MemPalace hooks: `mempalace/hooks/mempal_save_hook.sh`
- Claude Code hook API: `.claude/settings.json` format
- Sessions design: `docs/superpowers/specs/2026-04-29-sessions-design.md`
- Roadmap: `docs/mempalace-diff.md` §13
