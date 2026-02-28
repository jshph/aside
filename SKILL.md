---
name: aside
description: Align an aside session's timestamped memo with its audio transcript into an interleaved timeline artifact for synthesis. Use when the user has an aside session (memo + WAV) and wants to combine it with a transcript for downstream processing like /transcript.
argument-hint: <session-name> [--transcript path] [--output path]
user-invocable: true
allowed-tools: Bash, Read, Write, Glob, Grep, AskUserQuestion
---

# Aside — Memo + Transcript Alignment

Take an aside session's timestamped memo and its audio transcript, align them on a shared timeline, and produce an interleaved artifact that can be used for synthesis (e.g., by `/transcript`).

## What this produces

An intermediate markdown document that interleaves two streams by timestamp:

- **Transcript entries** — what was actually said (from aside.py or another transcription source)
- **Memo annotations** — what the user noted in real time during the session

This combined artifact preserves both the raw conversation and the user's live attention signal: what they found important enough to write down, and when.

## Arguments

`$ARGUMENTS` format: `<session-name> [--transcript <path>] [--output <path>]`

- **session-name** (required): The aside session name (e.g., `my-call`). Used to find:
  - Memo: `<session-name>.md` (in the aside working directory)
  - Audio: `recordings/<session-name>_seg*.wav`
  - DB: `recordings/.aside.db` (for segment offsets and durations)
- **--transcript** (optional): Path to an existing transcript file. If omitted, transcribes the WAV segments first using aside.py.
- **--output** (optional): Output path for the aligned artifact. Defaults to `<session-name>_aligned.md`.

### Parsing $ARGUMENTS

```
$ARGUMENTS = "standup"
→ session: "standup"
→ memo: "standup.md"
→ audio: "recordings/standup_seg*.wav"
→ transcript: (generate from WAV)
→ output: "standup_aligned.md"

$ARGUMENTS = "standup --transcript inbox/standup-transcript.md"
→ session: "standup"
→ transcript: "inbox/standup-transcript.md"
→ output: "standup_aligned.md"

$ARGUMENTS = "standup --output inbox/standup-combined.md"
→ session: "standup"
→ transcript: (generate from WAV)
→ output: "inbox/standup-combined.md"
```

## Execution

### Step 1: Locate session artifacts

1. Read the memo file `<session-name>.md` — contains lines like:
   ```
   [00:05] discussing API redesign
   [01:30 ~02:15] revisited auth approach — decided on JWT
   [05:00] action item: Josh to draft RFC by Friday
   ```
2. Query the session database for segment info:
   ```bash
   sqlite3 recordings/.aside.db "SELECT segment_index, wav_path, offset_ms, duration_secs FROM segments WHERE session_name = '<session-name>' ORDER BY segment_index"
   ```
3. List WAV segments: `recordings/<session-name>_seg*.wav`

If the memo file doesn't exist, ask the user for the correct session name.

### Step 2: Obtain transcript

**If `--transcript` was provided:**

Read the transcript file. Supported formats:
- **Hyprnote transcript.json**: Parse `transcripts[0].words[]`, each with `start_ms`, `end_ms`, `text`, `channel`
- **Timestamped markdown**: Lines like `[00:05] Speaker: text` or entries with `start_ms`/`end_ms` markers
- **Plain text with speaker labels**: `Speaker A: ...` / `Me: ...` — no timestamps, align by order only

**If no transcript provided, generate one:**

For each WAV segment:
```bash
python3 ~/Hacks/aside/aside.py "recordings/<session-name>_seg<N>.wav" --output "/tmp/<session-name>_seg<N>_transcript.json"
```

When there are multiple segments (from device switches), adjust transcript timestamps by each segment's `offset_ms` from the database so they align to the session's global timeline.

### Step 3: Parse both streams into a unified timeline

Build a list of timed events from both sources:

**From the memo** — each line becomes:
```
{ type: "memo", time_s: <seconds from session start>, text: "...", edited_at: <optional> }
```

Parse timestamps using the `[MM:SS]` or `[HH:MM:SS]` format. Lines with `~` have an edit timestamp indicating the note was revised.

**From the transcript** — each phrase/entry becomes:
```
{ type: "transcript", time_s: <start_ms / 1000>, end_s: <end_ms / 1000>, channel: 0|1, text: "..." }
```

Channel 0 = mic (the user), Channel 1 = system audio (the other person/people).

**Multi-segment alignment**: For segments beyond seg0, add `offset_ms / 1000` to all transcript timestamps from that segment to place them on the session's global timeline.

### Step 4: Generate the aligned artifact

Sort all events by timestamp. Write a markdown document with this structure:

```markdown
# <session-name> — Aligned Timeline

**Session start**: <datetime from DB>
**Duration**: <total duration>
**Segments**: <count> audio segments

---

## Timeline

### 00:00–00:30

> [transcript ch1] So the main question is whether we should redesign the auth layer or patch the existing one.

> [transcript ch0] Yeah, I think a full redesign makes more sense given the new requirements.

**[00:05 memo]** discussing API redesign

### 00:30–01:30

> [transcript ch1] The JWT approach would let us decouple the session store entirely...

> [transcript ch0] Right, and we could use refresh tokens to handle the mobile case.

### 01:30–02:30

> [transcript ch0] Actually let me reconsider — what about the migration path for existing sessions?

**[01:30 memo]** revisited auth approach — decided on JWT
*[edited at 02:15]*

### 05:00–05:30

> [transcript ch0] Okay so I'll draft an RFC for the JWT migration by end of week.

> [transcript ch1] Sounds good, I'll review it Monday.

**[05:00 memo]** action item: Josh to draft RFC by Friday

---

## Memo lines without nearby transcript

(Any memo lines that don't fall near transcript entries, listed here for completeness)

## Session metadata

- Segments: seg0 (0ms offset, 312.5s), seg1 (315000ms offset, 180.2s)
- Memo lines: 12
- Transcript entries: 47
```

**Formatting rules:**
- Group into time windows (30s–60s chunks, or natural breaks in conversation)
- Transcript lines are blockquotes with channel labels: `> [transcript ch0]` for the user, `> [transcript ch1]` for the other side
- Memo lines are bold with their timestamp: `**[MM:SS memo]** text`
- Edited memo lines get an italic annotation: `*[edited at MM:SS]*`
- If a memo line falls within a transcript window, place it after the transcript lines it corresponds to
- If a memo line has no nearby transcript (e.g., during silence), include it in sequence anyway

### Step 5: Write output

Write the aligned artifact to the output path. Report to the user:

> Aligned <N> memo lines with <M> transcript entries across <duration>. Output: `<path>`

## Relationship to /transcript

The aligned artifact from `/aside` is designed as **input** to `/transcript`. The workflow:

1. **Record**: Run `aside <session-name>` — take notes while recording
2. **Align**: `/aside <session-name>` — produces the interleaved timeline
3. **Synthesize**: `/transcript <aligned-artifact>.md` — processes into structured vault notes

The aligned artifact gives `/transcript` two signals it wouldn't otherwise have:
- **The user's real-time attention** — memo lines mark what the user considered important during the conversation, not in hindsight
- **Temporal context** — knowing *when* the user wrote a note relative to the conversation helps weight and structure the output

## Example Invocations

```
/aside standup
# Transcribe WAV segments and align with memo for the "standup" session

/aside planning --transcript inbox/planning-transcript.md
# Align memo with an existing transcript file

/aside planning --output inbox/planning-aligned.md
# Specify output location

/aside planning --transcript inbox/planning-transcript.md --output inbox/planning-aligned.md
# Both explicit transcript and output
```
