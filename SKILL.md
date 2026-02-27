---
name: aside
description: Transcribe stereo WAV audio into Hyprnote transcript.json format with speaker separation and cleanup. Use when the user provides a .wav file path or Hyprnote session directory and wants it transcribed. Triggers on "transcribe this audio", "aside", or references to Hyprnote session audio files.
user-invocable: true
allowed-tools: Bash, Read, AskUserQuestion
---

# Aside — Stereo WAV to Hyprnote Transcript

Transcribe stereo WAV audio (speaker-separated channels) into Hyprnote's `transcript.json` format with multi-pass cleanup.

## Arguments

`$ARGUMENTS` format: `<path> [--output <path>] [--keep-backchannels] [--model <repo>]`

- **path** (required): Path to a stereo `.wav` file or a Hyprnote session directory
  - Session directory: looks for `audio.wav` inside, writes `transcript.json` beside it
  - WAV file in a session dir (has `_meta.json` sibling): auto-writes `transcript.json` there
  - WAV file elsewhere: writes `<name>_transcript.json` next to it, or use `--output`
- **--output** (optional): Explicit output path, overrides auto-detection
- **--keep-backchannels** (optional): Skip backchannel/filler removal passes (keeps "yeah", "mm-hmm" etc.)
- **--model** (optional): Whisper model repo (default: `mlx-community/whisper-large-v3-turbo`)

## Execution

### Step 1: Parse arguments

Extract the path and flags from `$ARGUMENTS`:

```
PATH = first positional argument (the .wav file or session directory)
OUTPUT = value after --output, if present
KEEP_BC = true if --keep-backchannels present
MODEL = value after --model, if present
```

### Step 2: Validate input

Before running the script, verify:

1. The path exists (file or directory)
2. If a file, it ends in `.wav`
3. ffmpeg is available: `which ffmpeg`
4. mlx-whisper is installed: `python3 -c "import mlx_whisper" 2>&1`

If ffmpeg is missing, tell the user: `brew install ffmpeg`
If mlx-whisper is missing, tell the user: `pip3 install mlx-whisper`

### Step 3: Run the script

```bash
python3 ~/Hacks/aside/aside.py "$PATH" [--output "$OUTPUT"] [--keep-backchannels] [--model "$MODEL"]
```

This will take a while for long audio files. The script prints progress lines to stdout as it works.

### Step 4: Handle results

Parse the stdout protocol lines:

**On success (output contains `OUTPUT_FILE:`):**

- `SESSION_ID:<id>` — the Hyprnote session ID (if auto-detected)
- `DURATION:<seconds>` — audio duration
- `CLEANUP:raw_words=N` — raw word count from Whisper
- `CLEANUP:final_entries=N` — entry count after cleanup
- `OUTPUT_FILE:<path>` — where transcript.json was written

Report to the user:

> Transcribed <duration> of audio into <final_entries> entries (from <raw_words> raw words). Output: `<path>`

### Step 5: Error handling

**If output contains `ERROR:`:**

Report the error. Common issues:
- File not found or not a WAV
- ffmpeg not installed
- mlx-whisper not installed
- Mono file (needs stereo with speaker-separated channels)
- Insufficient disk space for temp files

## Example Invocations

```
/aside ~/Library/Application Support/hyprnote/sessions/027e6d28-ce6b-4ea6-8f06-48dd4d50fb50
# Transcribe a Hyprnote session by directory path

/aside /path/to/recording.wav
# Transcribe a standalone WAV file

/aside /path/to/recording.wav --output /tmp/transcript.json
# Explicit output path

/aside /path/to/recording.wav --keep-backchannels
# Keep all backchannels and fillers in the output

/aside /path/to/recording.wav --model mlx-community/whisper-large-v3
# Use a different Whisper model
```
