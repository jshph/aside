#!/usr/bin/env python3
"""Aside audio processing toolkit.

Subcommands:
    transcribe  Transcribe stereo WAV into Hyprnote transcript.json format.
    align       Align memo + transcript into timeline markdown.

Usage:
    python3 aside.py transcribe <wav_path> [--output <path>] [--keep-backchannels] [--model <path>]
    python3 aside.py align --memo <path> --transcripts <path>... --meta <path> --output <path>

Stdout protocol (machine-parseable lines):
    SPLITTING_CHANNELS          splitting stereo into mono
    TRANSCRIBING:0 / :1        transcribing each channel
    CLEANUP:raw_words=N         raw word count before cleanup
    CLEANUP:final_entries=N     entry count after cleanup
    OUTPUT_FILE:<path>          where output was written
    SESSION_ID:<id>             detected session ID
    DURATION:<seconds>          audio duration in seconds
    ALIGNED:memos=N,...         alignment summary
    ERROR:<message>             on failure
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import uuid
from datetime import datetime, timezone

DEFAULT_MODEL = "ggml-large-v3-turbo.bin"

# ---------------------------------------------------------------------------
# Backchannel / filler constants
# ---------------------------------------------------------------------------

BACKCHANNEL_WORDS = {
    "yeah", "yeah.", "mm-hmm", "mm-hmm.", "mhm", "mhm.", "mm", "hmm",
    "wow", "wow.", "nice", "nice.", "sure", "sure.", "right", "right.",
    "-hmm", "-hmm.", "uh-huh", "uh-huh.",
}

BACKCHANNEL_PHRASES = {
    "mm", "mm-hmm", "mm-hmm.", "-hmm", "-hmm.", "hmm", "hmm.",
    "mhm", "mhm.", "yeah", "yeah.", "uh-huh", "uh-huh.",
    "sure", "sure.", "nice", "nice.", "right", "right.",
    "wow", "wow.", "okay", "okay.", "oh", "oh.",
}

FILLER_PATTERN = re.compile(
    r"^[\s.,!?]*(mm[-\s]?hmm|mhm|mm|hmm|yeah|yep|wow|nice|sure|right|okay|oh|uh[-\s]?huh|ah|um|i|so)([.\s,!?]|\s)*$",
    re.IGNORECASE,
)

PURE_FILLER_RE = re.compile(
    r"(Mm-hmm|Mhm|Mm|Hmm|-hmm|Yeah|Yep|Wow|Nice|Sure|Right|Okay|Cool|Oh|Uh-huh)[.\s,!?]*",
    re.IGNORECASE,
)

LEADING_BACKCHANNEL_RE = re.compile(
    r"^(\s*(Mm-hmm|Mhm|Mm|Hmm|-hmm|Yeah|Yep|Wow|Nice|Sure|Right|Okay|Cool|Oh|Uh-huh)[.\s,!?]*)+",
    re.IGNORECASE,
)

# ---------------------------------------------------------------------------
# Audio helpers
# ---------------------------------------------------------------------------


def get_duration(path: str) -> float:
    """Return audio duration in seconds via ffprobe."""
    result = subprocess.run(
        [
            "ffprobe", "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            path,
        ],
        capture_output=True, text=True, check=True,
    )
    return float(result.stdout.strip())


def split_stereo(path: str) -> tuple[str, str]:
    """Split stereo WAV into two mono temp WAVs via ffmpeg. Returns (ch0, ch1)."""
    ch0 = tempfile.mktemp(suffix="_ch0.wav")
    ch1 = tempfile.mktemp(suffix="_ch1.wav")

    for out, chan in [(ch0, "c0"), (ch1, "c1")]:
        subprocess.run(
            [
                "ffmpeg", "-y", "-i", path,
                "-af", f"pan=mono|c0={chan}",
                "-ar", "16000", "-acodec", "pcm_s16le",
                out,
            ],
            check=True, capture_output=True,
        )

    return ch0, ch1


def _resolve_model_path(model: str) -> str:
    """Resolve a whisper-cli model path.

    If model is an absolute path, use it directly. Otherwise look in the
    conventional ~/.local/share/whisper-cpp/ directory.
    """
    if os.path.isabs(model) and os.path.isfile(model):
        return model

    conventional = os.path.expanduser(f"~/.local/share/whisper-cpp/{model}")
    if os.path.isfile(conventional):
        return conventional

    print(
        f"ERROR:Model not found: {model}\n"
        f"Download with: hf download ggerganov/whisper.cpp {model} "
        f"--local-dir ~/.local/share/whisper-cpp/",
        file=sys.stdout,
    )
    sys.exit(1)


def transcribe_channel(audio_path: str, channel: int, model: str) -> list[dict]:
    """Transcribe a mono WAV via whisper-cli and return word dicts."""
    model_path = _resolve_model_path(model)

    out_prefix = tempfile.mktemp(suffix="_whisper")

    try:
        subprocess.run(
            [
                "whisper-cli",
                "-m", model_path,
                "-f", audio_path,
                "-l", "en",
                "-ojf",
                "-of", out_prefix,
                "--max-context", "0",
            ],
            check=True, capture_output=True, text=True,
        )

        json_path = out_prefix + ".json"
        with open(json_path) as f:
            data = json.load(f)
    finally:
        for suffix in (".json", ""):
            try:
                os.unlink(out_prefix + suffix)
            except OSError:
                pass

    words = []
    for segment in data.get("transcription", []):
        for tok in segment.get("tokens", []):
            if tok.get("id", 0) >= 50000:
                continue
            text = tok.get("text", "")
            if text.startswith("["):
                continue
            offsets = tok.get("offsets", {})
            words.append({
                "channel": channel,
                "start_ms": offsets.get("from", 0),
                "end_ms": offsets.get("to", 0),
                "id": str(uuid.uuid4()),
                "text": text,
            })
    return words


# ---------------------------------------------------------------------------
# Multi-pass cleanup
# ---------------------------------------------------------------------------


SILENCE_HALLUCINATION_RE = re.compile(
    r"^\s*(thank\s+you\.?|thanks\s+for\s+watching\.?|please\s+subscribe\.?"
    r"|see\s+you\s+(next\s+time|in\s+the\s+next)\.?|bye[\s.!]*"
    r"|you\.?|\.+)\s*$",
    re.IGNORECASE,
)


def _remove_hallucinations(words: list[dict]) -> list[dict]:
    """Remove OpenOpen-style hallucination words (but keep real words like OpenClaw)."""
    out = []
    for w in words:
        text = w["text"].strip()
        if re.search(r"Open{2,}|OpenOpen", text):
            continue
        out.append(w)
    return out


def _remove_silence_hallucinations(entries: list[dict]) -> list[dict]:
    """Remove phrases that are classic Whisper silence hallucinations.

    Whisper hallucinates short phrases like 'Thank you.' or 'Thanks for
    watching.' when processing quiet/silent audio.  These are caught at the
    phrase level after word merging so multi-word hallucinations are matched.
    """
    return [e for e in entries if not SILENCE_HALLUCINATION_RE.match(e["text"].strip())]


def _clean_exclamation_artifacts(words: list[dict]) -> list[dict]:
    """Strip stray punctuation artifacts left by hallucinations."""
    out = []
    for w in words:
        text = w["text"]
        if text.strip() in ("!", "!!", "!!!", "!!!!"):
            continue
        text = re.sub(r"([.?])!+", r"\1", text)
        text = re.sub(r"^\s*!+", lambda m: m.group(0).replace("!", ""), text)
        text = re.sub(r"!+$", "", text)
        w = dict(w, text=text)
        out.append(w)
    return out


def _strip_trailing_open(words: list[dict]) -> list[dict]:
    """Remove trailing 'Open' artifacts from merged hallucination remnants."""
    out = []
    for w in words:
        w = dict(w, text=re.sub(r"Open$", "", w["text"]))
        if w["text"].strip():
            out.append(w)
    return out


def _dedup_consecutive(words: list[dict]) -> list[dict]:
    """Deduplicate consecutive repeated words per channel.

    Backchannel words: keep max 2 consecutive.
    Content words: keep max 1.
    """
    for ch in (0, 1):
        ch_indices = [i for i, w in enumerate(words) if w["channel"] == ch]
        to_remove = set()
        streak = 1
        for idx in range(1, len(ch_indices)):
            ci, pi = ch_indices[idx], ch_indices[idx - 1]
            ct = words[ci]["text"].strip().lower()
            pt = words[pi]["text"].strip().lower()
            if ct == pt:
                streak += 1
                limit = 2 if ct in BACKCHANNEL_WORDS else 1
                if streak > limit:
                    to_remove.add(ci)
            else:
                streak = 1
        words = [w for i, w in enumerate(words) if i not in to_remove]
    return words


def _merge_words_to_phrases(words: list[dict], gap_ms: int = 2000) -> list[dict]:
    """Merge consecutive same-channel words into phrase entries."""
    words.sort(key=lambda w: (w["start_ms"], w["channel"]))
    merged = []
    cur = None
    for w in words:
        if cur is None:
            cur = dict(w)
        elif w["channel"] == cur["channel"] and (w["start_ms"] - cur["end_ms"]) < gap_ms:
            cur["end_ms"] = w["end_ms"]
            cur["text"] += w["text"]
        else:
            merged.append(cur)
            cur = dict(w)
    if cur:
        merged.append(cur)
    return merged


def _merge_through_backchannels(entries: list[dict]) -> list[dict]:
    """Merge same-channel entries that are separated only by backchannels."""

    def _is_bc(e):
        return e["text"].strip().lower() in BACKCHANNEL_PHRASES

    merged = []
    i = 0
    while i < len(entries):
        turn = dict(entries[i])
        j = i + 1
        pending = []
        while j < len(entries):
            nxt = entries[j]
            if nxt["channel"] == turn["channel"]:
                for bc in pending:
                    merged.append(bc)
                pending = []
                turn["end_ms"] = nxt["end_ms"]
                turn["text"] += nxt["text"]
                j += 1
            elif _is_bc(nxt):
                pending.append(nxt)
                j += 1
            else:
                break
        merged.append(turn)
        for bc in pending:
            merged.append(bc)
        i = j

    merged.sort(key=lambda e: e["start_ms"])
    return merged


def _merge_same_channel(entries: list[dict], gap_ms: int = 3000) -> list[dict]:
    """Merge consecutive same-channel entries within gap_ms."""
    out = []
    cur = None
    for e in entries:
        if cur is None:
            cur = dict(e)
        elif e["channel"] == cur["channel"] and (e["start_ms"] - cur["end_ms"]) < gap_ms:
            cur["end_ms"] = e["end_ms"]
            cur["text"] += e["text"]
        else:
            out.append(cur)
            cur = dict(e)
    if cur:
        out.append(cur)
    return out


def _is_pure_filler(text: str) -> bool:
    """True if entire text is just backchannel/filler words."""
    stripped = PURE_FILLER_RE.sub("", text).strip()
    return not stripped


def _remove_pure_fillers(entries: list[dict]) -> list[dict]:
    """Remove entries that are entirely filler, then re-merge."""
    cleaned = [e for e in entries if not _is_pure_filler(e["text"].strip())]
    return _merge_same_channel(cleaned, gap_ms=5000)


def _is_fragment(entry: dict) -> bool:
    """Short, content-free snippet likely from overlap noise."""
    text = entry["text"].strip()
    if FILLER_PATTERN.match(text):
        return True
    words = text.split()
    if len(words) <= 3 and len(text) < 25:
        if text.endswith(("?", ".")) and text[0].isupper():
            return False
        return True
    return False


def _remove_fragments(entries: list[dict]) -> list[dict]:
    """Remove short fragment entries and re-merge."""
    merged = []
    i = 0
    while i < len(entries):
        e = entries[i]
        if _is_fragment(e):
            prev_ch = merged[-1]["channel"] if merged else None
            next_ch = entries[i + 1]["channel"] if i + 1 < len(entries) else None
            if prev_ch is not None and next_ch is not None and prev_ch == next_ch and prev_ch != e["channel"]:
                i += 1
                continue
            elif prev_ch == e["channel"] and merged:
                merged[-1]["end_ms"] = e["end_ms"]
                merged[-1]["text"] += e["text"]
                i += 1
                continue
        merged.append(dict(e))
        i += 1

    return _merge_same_channel(merged, gap_ms=5000)


def _strip_leading_backchannel(text: str) -> str:
    """Remove backchannel prefix from text with real content after it."""
    cleaned = LEADING_BACKCHANNEL_RE.sub(" ", text).strip()
    return cleaned if cleaned else text


def _bridge_merge(entries: list[dict]) -> list[dict]:
    """Remove pure backchannel entries, strip backchannel prefixes, then bridge-merge
    same-channel entries separated by short other-channel interjections."""

    def _word_count(text):
        return len(text.strip().split())

    # Strip pure backchannels and clean prefixes
    cleaned = []
    for e in entries:
        text = e["text"].strip()
        if _is_pure_filler(text):
            continue
        stripped = _strip_leading_backchannel(e["text"])
        if stripped != e["text"].strip():
            e = dict(e, text=" " + stripped)
        cleaned.append(e)

    # Bridge merge per channel
    for ch in (0, 1):
        ch_indices = [i for i, e in enumerate(cleaned) if e["channel"] == ch]
        merge_pairs = []
        for idx in range(len(ch_indices) - 1):
            ci, cj = ch_indices[idx], ch_indices[idx + 1]
            between = cleaned[ci + 1:cj]
            gap = cleaned[cj]["start_ms"] - cleaned[ci]["end_ms"]
            if gap < 10000 and all(_word_count(b["text"]) <= 5 for b in between):
                merge_pairs.append((ci, cj, list(range(ci + 1, cj))))

        absorbed = set()
        for ci, cj, between_idx in reversed(merge_pairs):
            if ci in absorbed or cj in absorbed:
                continue
            cleaned[ci]["end_ms"] = cleaned[cj]["end_ms"]
            cleaned[ci]["text"] += cleaned[cj]["text"]
            absorbed.add(cj)
            for bi in between_idx:
                absorbed.add(bi)
        cleaned = [e for i, e in enumerate(cleaned) if i not in absorbed]

    # Final same-channel adjacency merge
    out = []
    for e in cleaned:
        if out and e["channel"] == out[-1]["channel"]:
            out[-1]["end_ms"] = e["end_ms"]
            out[-1]["text"] += e["text"]
        else:
            out.append(dict(e))
    return out


def cleanup(words: list[dict], keep_backchannels: bool = False) -> list[dict]:
    """Run the full multi-pass cleanup pipeline on raw word entries."""
    # Word-level cleanup
    words = _remove_hallucinations(words)
    words = _clean_exclamation_artifacts(words)
    words = _strip_trailing_open(words)
    words = _dedup_consecutive(words)

    # Merge words into phrases
    entries = _merge_words_to_phrases(words, gap_ms=2000)
    entries = _remove_silence_hallucinations(entries)

    if keep_backchannels:
        return _merge_same_channel(entries, gap_ms=3000)

    # Phrase-level cleanup
    entries = _merge_through_backchannels(entries)
    entries = _merge_same_channel(entries, gap_ms=3000)
    entries = _remove_pure_fillers(entries)
    entries = _remove_fragments(entries)
    entries = _bridge_merge(entries)

    return entries


# ---------------------------------------------------------------------------
# Output formatting
# ---------------------------------------------------------------------------


def format_hyprnote(entries: list[dict], session_id: str | None = None) -> dict:
    """Wrap cleaned entries into Hyprnote transcript.json structure."""
    now = datetime.now(timezone.utc)
    created_at = now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond // 1000:03d}Z"
    started_at = int(now.timestamp() * 1000)

    return {
        "transcripts": [
            {
                "created_at": created_at,
                "id": str(uuid.uuid4()),
                "session_id": session_id or str(uuid.uuid4()),
                "speaker_hints": [],
                "started_at": started_at,
                "user_id": "00000000-0000-0000-0000-000000000000",
                "words": entries,
            }
        ]
    }


# ---------------------------------------------------------------------------
# Path resolution
# ---------------------------------------------------------------------------

HYPRNOTE_SESSIONS = os.path.expanduser(
    "~/Library/Application Support/hyprnote/sessions"
)


def resolve_paths(input_path: str, output_flag: str | None) -> tuple[str, str, str | None]:
    """Resolve input WAV path and output path.

    Returns (wav_path, output_path, session_id).
    """
    input_path = os.path.expanduser(input_path)

    # If input is a directory, look for audio.wav inside it
    if os.path.isdir(input_path):
        wav = os.path.join(input_path, "audio.wav")
        if not os.path.exists(wav):
            raise FileNotFoundError(f"No audio.wav in {input_path}")
        session_id = os.path.basename(input_path)
        out = output_flag or os.path.join(input_path, "transcript.json")
        return wav, out, session_id

    # Input is a file
    if not os.path.isfile(input_path):
        raise FileNotFoundError(f"File not found: {input_path}")

    parent = os.path.dirname(input_path)
    meta = os.path.join(parent, "_meta.json")
    session_id = None

    if os.path.exists(meta):
        session_id = os.path.basename(parent)
        out = output_flag or os.path.join(parent, "transcript.json")
    elif output_flag:
        out = output_flag
    else:
        out = os.path.splitext(input_path)[0] + "_transcript.json"

    return input_path, out, session_id


# ---------------------------------------------------------------------------
# Alignment: memo + transcript → timeline
# ---------------------------------------------------------------------------

MEMO_LINE_RE = re.compile(
    r'^\[(?:(\d+):)?(\d+):(\d+)'
    r'(?:\s*~\s*(?:(\d+):)?(\d+):(\d+))?'
    r'\]\s*(.*)',
)


def _parse_ts(h: str | None, m: str, s: str) -> int:
    """Convert optional-hours, minutes, seconds strings to total seconds."""
    return (int(h) * 3600 if h else 0) + int(m) * 60 + int(s)


def _fmt_ts(seconds: float) -> str:
    """Format seconds as MM:SS or HH:MM:SS."""
    t = int(seconds)
    if t >= 3600:
        return f"{t // 3600}:{(t % 3600) // 60:02d}:{t % 60:02d}"
    return f"{t // 60:02d}:{t % 60:02d}"


def parse_memo(path: str) -> list[dict]:
    """Parse memo file with [MM:SS] / [HH:MM:SS] / [MM:SS ~MM:SS] timestamps.

    Returns list of {time_s, edited_at_s, text} dicts.
    """
    entries = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            m = MEMO_LINE_RE.match(line)
            if not m:
                continue
            time_s = _parse_ts(m.group(1), m.group(2), m.group(3))
            edited_at_s = None
            if m.group(5) is not None:
                edited_at_s = _parse_ts(m.group(4), m.group(5), m.group(6))
            entries.append({
                'time_s': time_s,
                'edited_at_s': edited_at_s,
                'text': m.group(7).strip(),
            })
    return entries


def parse_transcripts(paths: list[str], meta: dict) -> list[dict]:
    """Read transcript JSONs, apply segment offsets from meta.

    Returns list of {time_s, end_s, channel, text} dicts.
    """
    offsets = {s['segment_index']: s.get('offset_ms', 0)
               for s in meta.get('segments', [])}
    entries = []
    for i, path in enumerate(paths):
        offset_ms = offsets.get(i, 0)
        with open(path) as f:
            data = json.load(f)
        for word in data.get('transcripts', [{}])[0].get('words', []):
            entries.append({
                'time_s': (word['start_ms'] + offset_ms) / 1000.0,
                'end_s': (word['end_ms'] + offset_ms) / 1000.0,
                'channel': word['channel'],
                'text': word['text'].strip(),
            })
    return entries


def build_timeline(memos: list[dict], transcripts: list[dict]) -> list[dict]:
    """Merge memo and transcript events, sort by timestamp."""
    events = []
    for m in memos:
        events.append({
            'type': 'memo',
            'time_s': m['time_s'],
            'end_s': m['time_s'],
            'edited_at_s': m.get('edited_at_s'),
            'text': m['text'],
        })
    for t in transcripts:
        events.append({
            'type': 'transcript',
            'time_s': t['time_s'],
            'end_s': t['end_s'],
            'channel': t['channel'],
            'text': t['text'],
        })
    # Memos sort after transcript entries at the same timestamp
    events.sort(key=lambda e: (e['time_s'], e['type'] == 'memo'))
    return events


def group_into_windows(events: list[dict], max_gap_s: int = 60) -> list[dict]:
    """Group events into time windows.

    Memo timestamps create window boundaries. Gaps >max_gap_s without memos
    get subdivided.

    Returns list of {start_s, end_s, events} dicts.
    """
    if not events:
        return []

    session_end = max(e['end_s'] for e in events)
    memo_times = sorted(set(e['time_s'] for e in events if e['type'] == 'memo'))

    # Boundaries: session start + memo times + session end
    boundaries = sorted(set([0.0] + memo_times + [session_end]))

    # Subdivide gaps longer than max_gap_s
    refined = [boundaries[0]]
    for i in range(1, len(boundaries)):
        gap = boundaries[i] - refined[-1]
        if gap > max_gap_s:
            n = max(1, round(gap / max_gap_s))
            step = gap / n
            for j in range(1, n):
                refined.append(round(refined[-1] + step, 1))
        refined.append(boundaries[i])
    refined = sorted(set(refined))

    # Assign events to windows [start, next_boundary)
    windows = []
    for i in range(len(refined) - 1):
        w_start, w_end = refined[i], refined[i + 1]
        w_events = [e for e in events if w_start <= e['time_s'] < w_end]
        if w_events:
            windows.append({
                'start_s': w_start,
                'end_s': w_end,
                'events': w_events,
            })

    # Events at exactly session_end go into last window
    tail = [e for e in events if e['time_s'] >= refined[-1]]
    if tail:
        if windows:
            windows[-1]['events'].extend(tail)
        else:
            windows.append({
                'start_s': refined[-1],
                'end_s': session_end,
                'events': tail,
            })

    return windows


def format_aligned_markdown(
    windows: list[dict], session_name: str, meta: dict,
) -> tuple[str, int, int, int]:
    """Render aligned timeline as markdown.

    Returns (text, memo_count, transcript_count, window_count).
    """
    lines = [f"# {session_name} — Aligned Timeline", ""]

    start_time = meta.get('start_time', '')
    if start_time:
        lines.append(f"**Session start**: {start_time}")

    segments = meta.get('segments', [])
    total_duration = sum(s.get('duration_secs', 0) for s in segments)
    if total_duration:
        m, s = divmod(int(total_duration), 60)
        lines.append(f"**Duration**: {m}m {s}s")

    seg_count = len(segments)
    lines.append(f"**Segments**: {seg_count} audio segment{'s' if seg_count != 1 else ''}")
    lines.extend(["", "---", "", "## Timeline", ""])

    memo_count = 0
    transcript_count = 0

    for w in windows:
        lines.append(f"### {_fmt_ts(w['start_s'])}–{_fmt_ts(w['end_s'])}")
        lines.append("")
        for e in w['events']:
            if e['type'] == 'transcript':
                transcript_count += 1
                lines.append(f"> [transcript ch{e['channel']}] {e['text']}")
                lines.append("")
            elif e['type'] == 'memo':
                memo_count += 1
                lines.append(f"**[{_fmt_ts(e['time_s'])} memo]** {e['text']}")
                if e.get('edited_at_s') is not None:
                    lines.append(f"*[edited at {_fmt_ts(e['edited_at_s'])}]*")
                lines.append("")

    lines.extend(["---", "", "## Session metadata", ""])
    for seg in segments:
        idx = seg['segment_index']
        offset = seg.get('offset_ms', 0)
        dur = seg.get('duration_secs', 0)
        lines.append(f"- seg{idx} ({offset}ms offset, {dur:.1f}s)")
    lines.append(f"- Memo lines: {memo_count}")
    lines.append(f"- Transcript entries: {transcript_count}")
    lines.append("")

    return "\n".join(lines), memo_count, transcript_count, len(windows)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def cmd_transcribe(args):
    """Execute the transcribe subcommand."""
    try:
        wav_path, out_path, session_id = resolve_paths(args.input, args.output)
    except FileNotFoundError as e:
        print(f"ERROR:{e}", file=sys.stdout)
        sys.exit(1)

    if session_id:
        print(f"SESSION_ID:{session_id}")

    try:
        duration = get_duration(wav_path)
        print(f"DURATION:{duration:.1f}")
    except Exception:
        duration = None

    print("SPLITTING_CHANNELS")
    try:
        ch0_path, ch1_path = split_stereo(wav_path)
    except subprocess.CalledProcessError as e:
        print(f"ERROR:ffmpeg failed: {e.stderr.decode() if e.stderr else e}")
        sys.exit(1)

    try:
        print("TRANSCRIBING:0")
        ch0_words = transcribe_channel(ch0_path, 0, args.model)
        print("TRANSCRIBING:1")
        ch1_words = transcribe_channel(ch1_path, 1, args.model)
    except Exception as e:
        print(f"ERROR:Transcription failed: {e}")
        sys.exit(1)
    finally:
        for p in (ch0_path, ch1_path):
            try:
                os.unlink(p)
            except OSError:
                pass

    all_words = ch0_words + ch1_words
    print(f"CLEANUP:raw_words={len(all_words)}")

    entries = cleanup(all_words, keep_backchannels=args.keep_backchannels)
    print(f"CLEANUP:final_entries={len(entries)}")

    transcript = format_hyprnote(entries, session_id)
    with open(out_path, "w") as f:
        json.dump(transcript, f, indent=2)

    print(f"OUTPUT_FILE:{out_path}")


def cmd_align(args):
    """Execute the align subcommand."""
    try:
        memos = parse_memo(args.memo)
    except FileNotFoundError:
        print(f"ERROR:Memo file not found: {args.memo}")
        sys.exit(1)

    try:
        with open(args.meta) as f:
            meta = json.load(f)
    except FileNotFoundError:
        print(f"ERROR:Meta file not found: {args.meta}")
        sys.exit(1)

    try:
        transcripts = parse_transcripts(args.transcripts, meta)
    except FileNotFoundError as e:
        print(f"ERROR:Transcript file not found: {e}")
        sys.exit(1)

    timeline = build_timeline(memos, transcripts)
    windows = group_into_windows(timeline)

    session_name = meta.get('name', os.path.splitext(os.path.basename(args.memo))[0])
    md, memo_count, transcript_count, window_count = format_aligned_markdown(
        windows, session_name, meta,
    )

    out_dir = os.path.dirname(args.output)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(args.output, "w") as f:
        f.write(md)

    print(f"ALIGNED:memos={memo_count},transcripts={transcript_count},windows={window_count}")
    print(f"OUTPUT_FILE:{args.output}")


def main():
    parser = argparse.ArgumentParser(description="Aside audio processing toolkit.")
    subparsers = parser.add_subparsers(dest="command")

    # transcribe subcommand
    p_transcribe = subparsers.add_parser(
        "transcribe",
        help="Transcribe stereo WAV into Hyprnote transcript.json format.",
    )
    p_transcribe.add_argument("input", help="Path to stereo .wav file or Hyprnote session directory")
    p_transcribe.add_argument("--output", "-o", help="Output path (default: auto-detect)")
    p_transcribe.add_argument("--keep-backchannels", action="store_true",
                              help="Skip backchannel/filler removal passes")
    p_transcribe.add_argument("--model", default=DEFAULT_MODEL,
                              help=f"Whisper model repo (default: {DEFAULT_MODEL})")

    # align subcommand
    p_align = subparsers.add_parser(
        "align",
        help="Align memo + transcript into timeline markdown.",
    )
    p_align.add_argument("--memo", required=True, help="Path to memo .md file")
    p_align.add_argument("--transcripts", required=True, nargs="+",
                         help="Path(s) to transcript JSON files")
    p_align.add_argument("--meta", required=True, help="Path to session .meta.json")
    p_align.add_argument("--output", required=True,
                         help="Output path for aligned markdown")

    args = parser.parse_args()

    if args.command == "transcribe":
        cmd_transcribe(args)
    elif args.command == "align":
        cmd_align(args)
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
