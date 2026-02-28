---
name: aside
description: End-to-end aside session processing — transcribe, align memo + transcript, distill into a structured vault note via Enzyme.
argument-hint: <session-name> [--template name] [--prep path] [--transcript path] [--align-only]
user-invocable: true
allowed-tools: Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion, mcp__enzyme__semantic_search, mcp__enzyme__start_exploring_vault
---

# Aside — Capture to Vault Note

Take an aside session end-to-end: transcribe audio, align the transcript with the user's real-time memo, distill into a structured vault note connected to existing thinking via Enzyme.

## What this produces

1. **Aligned timeline** (`.aside/<session>_aligned.md`) — interleaved transcript + memo on a shared timeline
2. **Vault note** — structured note written to the obsidian vault using a template, with Enzyme-sourced connections

If `--align-only` is passed, only the aligned timeline (step 1) is produced.

## Arguments

`$ARGUMENTS` format: `<session-name> [--template <name>] [--prep <path>] [--transcript <path>] [--align-only]`

- **session-name** (required): The aside session name (e.g., `my-call`). Used to find:
  - Memo: `<session-name>.md` (in the aside working directory)
  - Audio: `.aside/<session-name>_seg*.wav`
  - DB: `.aside/.aside.db` (for segment offsets and durations)
- **--template** (optional): Template name for the vault note (default: `1on1-idea-exchange`). Templates live in `/Users/joshuapham/obsidian/.claude/commands/transcript/templates/`.
- **--prep** (optional): Path to prep notes file for additional context during distillation.
- **--transcript** (optional): Path to an existing transcript file. If omitted, transcribes the WAV segments using aside.py.
- **--align-only** (optional): Stop after producing the aligned timeline. Skip distillation.

### Parsing $ARGUMENTS

```
$ARGUMENTS = "standup"
-> session: "standup"
-> memo: "standup.md"
-> audio: ".aside/standup_seg*.wav"
-> transcript: (generate from WAV)
-> template: "1on1-idea-exchange" (default)
-> output: ".aside/standup_aligned.md"

$ARGUMENTS = "standup --transcript inbox/standup-transcript.md --template discovery-call"
-> session: "standup"
-> transcript: "inbox/standup-transcript.md"
-> template: "discovery-call"
-> output: ".aside/standup_aligned.md"

$ARGUMENTS = "standup --align-only"
-> session: "standup"
-> transcript: (generate from WAV)
-> output: ".aside/standup_aligned.md"
-> STOP after Phase I

$ARGUMENTS = "standup --prep inbox/standup-prep.md"
-> session: "standup"
-> prep: "inbox/standup-prep.md"
-> full pipeline
```

---

## Phase I — Alignment

### Step 1: Locate session artifacts

1. Read the memo file `<session-name>.md` — contains lines like:
   ```
   [00:05] discussing API redesign
   [01:30 ~02:15] revisited auth approach — decided on JWT
   [05:00] action item: Josh to draft RFC by Friday
   ```
2. Query the session database for segment info:
   ```bash
   sqlite3 .aside/.aside.db "SELECT segment_index, wav_path, offset_ms, duration_secs FROM segments WHERE session_name = '<session-name>' ORDER BY segment_index"
   ```
3. List WAV segments: `.aside/<session-name>_seg*.wav`

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
python3 ~/Hacks/aside/aside.py ".aside/<session-name>_seg<N>.wav" --output "/tmp/<session-name>_seg<N>_transcript.json"
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

Write the aligned artifact to `.aside/<session-name>_aligned.md`. Report to the user:

> Aligned <N> memo lines with <M> transcript entries across <duration>. Output: `.aside/<session-name>_aligned.md`

**If `--align-only` was passed, stop here.**

---

## Phase II — Distillation

### Step 5: Memo-guided analysis

The memo is the user's real-time attention signal — what they found important enough to write down during the conversation, not in hindsight. Use it to drive analysis priority.

1. **Extract topics from memo lines first.** Each memo line marks a moment the user chose to note. Classify each line:
   - **Decision** — a choice was made ("decided on JWT")
   - **Action item** — a commitment or next step ("Josh to draft RFC by Friday")
   - **Insight** — a realization or interesting framing ("the real bottleneck is onboarding, not retention")
   - **Tension** — a disagreement or unresolved question ("revisited auth approach")
   - **Question** — something to follow up on
   - **Observation** — neutral notation of what's being discussed

2. **Edited memos (`~` timestamps) signal reconsideration.** A memo line with `[01:30 ~02:15]` means the user wrote something at 01:30 and came back to edit it at 02:15. This indicates the topic was important enough to revisit. Weight these higher.

3. **Extract topics from un-noted transcript.** Scan the transcript for significant topics that the user did *not* memo. These are secondary — the user may have chosen not to note them for a reason, or they may have been too absorbed to write.

4. **Build a prioritized topic list.** Memo-marked topics first (ordered by classification weight: decisions > action items > insights > tensions > questions > observations), then significant un-noted topics.

If `--prep` was provided, read the prep notes and compare: what the user came in wanting vs. what actually happened. Note gaps and surprises.

### Step 6: Enzyme vault search

Connect the conversation to existing vault thinking.

#### Phase A: Explore the vault

Run `mcp__enzyme__start_exploring_vault` first. This returns the **slate** — trending entities with catalysts that represent where the vault has already found language for things. Use the slate to calibrate search queries: if the transcript discusses "knowledge management tools" but the vault uses "pkm" or "tool-thinking", reach for the vault's language.

#### Phase B: Search for connections

Use **two search strategies**:

**Structured search (Grep)** — for concrete anchors that exist verbatim in the vault:
- People mentioned: `[[John Borthwick]]`, `[[Chris Perry]]`
- Tags from the slate that match transcript topics: `#pkm`, `#ai-ux`
- Companies or proper nouns: "Betaworks", "Readwise"
- Wikilinks or note titles

Run Grep for each concrete anchor. Prioritize anchors that appear near memo-marked topics.

**Semantic search** — for themes and concepts without a concrete anchor:
- Formulate 2-3 queries from the prioritized topic list (Step 5), using the vault's vocabulary where possible
- Focus on memo-marked topics first, then significant un-noted topics
- Queries should be substantive and specific, drawn from actual conversation content

**Good queries** (drawn from specific themes, calibrated to vault language):
- "happenstance interfaces and serendipity in knowledge tools"
- "creative tool vs consumer tool positioning"
- "behavioral graph as enabler business"

**Bad queries** (generic):
- "meeting notes"
- "conversation summary"
- "knowledge management"

For each query, run `mcp__enzyme__semantic_search` with `result_limit: 5`.

#### Phase C: Read and collect

After both structured and semantic results come back:
- Read the top 3-5 most relevant notes
- Note **existing tags** that appear in those notes (for use in the output — never invent tags)
- Note **people links** (`[[Person Name]]`) that appear
- Note **connections** between the transcript content and vault content — these become citations in the draft

### Step 7: Template + draft

1. **Load template** from `/Users/joshuapham/obsidian/.claude/commands/transcript/templates/`. Available templates:
   - `1on1-idea-exchange` (default) — for idea-rich 1:1 conversations
   - `discovery-call` — for client/prospect calls
   - `group-conversation` — for multi-person discussions

   If the requested template doesn't exist, list available templates and ask the user to choose.

2. **Generate draft** following these principles:

   **Topic ordering**: Use the memo-weighted priority from Step 5. Decisions and action items surface first, then insights and tensions, then un-noted topics. This reflects what the user actually cared about during the conversation.

   **Content principles:**
   - Preserve specific language and direct quotes — use their actual words
   - Reconstruct fragmented speech-to-text into intended meaning; flag uncertain reconstructions with [reconstructed]
   - Weave in vault context as natural `[[wikilink]]` citations where connections exist
   - Populate frontmatter with tags extracted from Enzyme results only (never invent tags)
   - Add people to the `people:` field using `[[Name]]` format
   - Skip pleasantries, logistics, and small talk unless they contained real content
   - Prioritize specificity over comprehensiveness — five vivid points beat fifteen generic bullets

   **Writing style:**
   - Direct statements over contrast constructions (no "doesn't X, but Y" patterns)
   - Use em dashes sparingly
   - No rhetorical questions as transitions
   - Avoid AI-typical phrases: "disappears into the background", "perhaps the better question is", "conceived as"
   - Active voice, concrete language, varied sentence construction

   **Citation integration:**
   - Reference vault notes naturally: `as explored in [[note title]]` or `connects to [[note title]]`
   - Use block embeds `![[file#^block-id]]` only when the source has explicit block IDs and the quote is concise and directly relevant
   - Don't force connections — only cite where the link genuinely enriches the note

### Step 8: Review

Present the complete draft to the user. Ask:
- Does the structure capture what mattered in this conversation?
- Any sections to expand, trim, or restructure?
- Any quotes or moments missing that should be included?

Apply revisions if requested. Iterate until the user is satisfied.

### Step 9: Write to vault

Once approved:

1. Create the note via `./scripts/new-note.sh` (run from `/Users/joshuapham/obsidian/`)
2. Populate with the approved content using the Edit tool
3. Rename with a descriptive suffix following vault naming conventions:
   - Keep timestamp prefix
   - Add 3-7 word descriptive name, lowercase
   - Pattern for conversations: `[timestamp] chat with [person] about [topic].md`

```bash
mv "/Users/joshuapham/obsidian/inbox/[timestamp].md" "/Users/joshuapham/obsidian/inbox/[timestamp] [descriptive name].md"
```

---

## Handling Poor Transcript Quality

Many transcripts come from speech-to-text and contain fragmented, garbled text. When you encounter this:

- Reconstruct the most likely intended meaning from context
- Use memo lines and prep notes (if available) to disambiguate unclear passages
- Preserve distinctive phrasing even when surrounding text is garbled
- If a passage is genuinely unrecoverable, note it as `[unclear]` rather than guessing
- Don't reproduce speech-to-text artifacts ("I. Mean. That. The.") — clean them up

## Example Invocations

```
/aside standup
# Full pipeline: transcribe, align, distill into vault note

/aside standup --align-only
# Only produce the aligned timeline, skip distillation

/aside planning --transcript inbox/planning-transcript.md
# Align with an existing transcript, then distill

/aside planning --template discovery-call --prep inbox/planning-prep.md
# Full pipeline with specific template and prep notes

/aside planning --transcript inbox/planning-transcript.md --align-only
# Align an existing transcript with memo, skip distillation
```
