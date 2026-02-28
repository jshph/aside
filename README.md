# aside

Record meetings with a timestamped notepad running alongside. Then turn the recording + your notes into a vault-connected artifact that beats what the transcript can do alone.

## Why this exists

Transcripts are noisy. They capture everything said but nothing about what mattered. Meeting notes are the opposite — they capture what you noticed, but miss the surrounding context. And neither one connects back to the thinking you've already done.

Aside fixes all three problems. It merges your real-time notes with the transcript on a shared timeline — your note at `[05:12] action item: draft RFC` gets interleaved with what was actually being discussed at 5:12. Then a Claude Code skill searches your Obsidian vault for related thinking, surfaces connections you wouldn't have made manually, and distills everything into a structured note with `[[wikilinks]]` woven in.

The transcript fills in what you didn't write down. Your notes tell the transcript what to foreground. And your vault gives the whole thing context that no recording app has access to.

Born from two years of the same Obsidian workflow: record, take sparse notes, transcribe, manually stitch the two together, then hunt through old notes for connections. This automates all of it.

## What it does

1. **Records** stereo audio (mic + system audio) while you type timestamped notes in a terminal editor
2. **Transcribes** via whisper.cpp with multi-pass cleanup (hallucination removal, dedup, filler stripping)
3. **Aligns** transcript and memo on a shared timeline
4. **Distills** into a structured vault note with connections to your existing notes via [Enzyme](https://github.com/jmpaz/enzyme)

Steps 1 is the Rust binary. Steps 2–4 are a Python script + Claude Code skill.

## Install

```bash
# The recorder
cargo install --path .

# The transcriber
brew install whisper-cpp ffmpeg

# Download the whisper model (~1.5 GB, one-time)
hf download ggerganov/whisper.cpp ggml-large-v3-turbo.bin \
  --local-dir ~/.local/share/whisper-cpp/
```

macOS only. Requires screen recording permission for system audio capture.

## Usage

### Record

```bash
aside standup           # new session — opens TUI editor + starts recording
aside --resume standup  # resume an existing session
aside --list            # list all sessions
```

The TUI is a timestamped notepad. Each line gets a `[MM:SS]` timestamp when you start typing it. Edit a line later and it shows `[MM:SS ~MM:SS]`.

Keybindings: `Ctrl+D` switch mic, `Ctrl+S` save, `Ctrl+C` quit and save.

On quit, the memo is published to your Obsidian vault if `.aside/config.toml` is configured.

### Transcribe + align

```bash
python3 aside.py .aside/standup_seg0.wav --output .aside/
```

Or use the `/aside` Claude Code skill for the full pipeline:

```
/aside standup              # transcribe → align → distill → vault note
/aside standup --align-only # just transcribe and align, no distillation
```

## Vault integration

Optional. Create `.aside/config.toml`:

```toml
[vault]
path = "~/obsidian"
folder = "inbox"
filename = "{{date:%Y-%m-%d-%-H-%M-%S}}"
open_in_obsidian = true
```

A template at `.aside/template.md` controls the note format. Variables: `{{name}}`, `{{memo}}`, `{{date:FORMAT}}`, `{{duration}}`.

## How it works

**Recording**: The Rust binary captures mic audio via cpal and system audio via Core Audio tap, writing 48kHz stereo WAV. You can switch mic devices mid-session — each device switch creates a new audio segment with proper timeline offsets.

**Transcription**: `aside.py` splits stereo into mono channels, transcribes each with `whisper-cli`, then runs cleanup passes: hallucination removal, consecutive word dedup, backchannel/filler stripping, and gap-based phrase merging. Outputs Hyprnote-format JSON.

**Alignment**: The `/aside` skill interleaves transcript segments with memo lines on a shared millisecond timeline. Memo lines act as attention signals — they tell the distillation step what the meeting participant thought was worth writing down in the moment.

**Distillation**: Searches the Obsidian vault via Enzyme for connections to the meeting content, weighted by what the memo flagged. Produces a structured note with `[[wikilinks]]` to existing thinking.

## Project structure

```
aside.py            Transcription + cleanup pipeline
src/
  main.rs           CLI and session orchestration
  recorder.rs       Stereo audio capture (mic + system)
  app.rs            Timestamped editor state
  tui.rs            Terminal UI (ratatui)
  session.rs        Session metadata (JSON)
  publish.rs        Vault note creation on session end
  parser.rs         Markdown ↔ editor round-tripping
  text_helpers.rs   Word/char boundary helpers
SKILL.md            Claude Code skill for the full pipeline
```

Sessions are stored in `.aside/` as WAV segments + JSON metadata + markdown memo.
