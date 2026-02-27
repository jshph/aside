#!/usr/bin/env python3
"""Transcribe a stereo WAV file into Hyprnote transcript.json format.

Splits stereo audio into per-channel mono files, transcribes each with
mlx-whisper, then runs multi-pass cleanup to remove hallucinations,
deduplicate words, strip backchannels/fillers, and merge overlapping
speech into clean turn-taking entries.

Usage:
    python3 aside.py <wav_path> [--output <path>] [--keep-backchannels] [--model <repo>]

Stdout protocol (machine-parseable lines):
    SPLITTING_CHANNELS          splitting stereo into mono
    TRANSCRIBING:0 / :1        transcribing each channel
    CLEANUP:raw_words=N         raw word count before cleanup
    CLEANUP:final_entries=N     entry count after cleanup
    OUTPUT_FILE:<path>          where transcript.json was written
    SESSION_ID:<id>             detected session ID
    DURATION:<seconds>          audio duration in seconds
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

DEFAULT_MODEL = "mlx-community/whisper-large-v3-turbo"

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


def transcribe_channel(audio_path: str, channel: int, model: str) -> list[dict]:
    """Transcribe a mono WAV and return word dicts in Hyprnote schema."""
    import mlx_whisper  # defer import so --help works without the dep

    result = mlx_whisper.transcribe(
        audio_path,
        path_or_hf_repo=model,
        word_timestamps=True,
        language="en",
    )

    words = []
    for seg in result.get("segments", []):
        for w in seg.get("words", []):
            words.append({
                "channel": channel,
                "end_ms": int(w["end"] * 1000),
                "id": str(uuid.uuid4()),
                "start_ms": int(w["start"] * 1000),
                "text": w["word"],
            })
    return words


# ---------------------------------------------------------------------------
# Multi-pass cleanup
# ---------------------------------------------------------------------------


def _remove_hallucinations(words: list[dict]) -> list[dict]:
    """Remove OpenOpen-style hallucination words (but keep real words like OpenClaw)."""
    out = []
    for w in words:
        text = w["text"].strip()
        if re.search(r"Open{2,}|OpenOpen", text):
            continue
        out.append(w)
    return out


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
# CLI
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(
        description="Transcribe stereo WAV into Hyprnote transcript.json format.",
    )
    parser.add_argument("input", help="Path to stereo .wav file or Hyprnote session directory")
    parser.add_argument("--output", "-o", help="Output path (default: auto-detect)")
    parser.add_argument("--keep-backchannels", action="store_true",
                        help="Skip backchannel/filler removal passes")
    parser.add_argument("--model", default=DEFAULT_MODEL,
                        help=f"Whisper model repo (default: {DEFAULT_MODEL})")
    args = parser.parse_args()

    try:
        wav_path, out_path, session_id = resolve_paths(args.input, args.output)
    except FileNotFoundError as e:
        print(f"ERROR:{e}", file=sys.stdout)
        sys.exit(1)

    if session_id:
        print(f"SESSION_ID:{session_id}")

    # Duration
    try:
        duration = get_duration(wav_path)
        print(f"DURATION:{duration:.1f}")
    except Exception:
        duration = None

    # Split channels
    print("SPLITTING_CHANNELS")
    try:
        ch0_path, ch1_path = split_stereo(wav_path)
    except subprocess.CalledProcessError as e:
        print(f"ERROR:ffmpeg failed: {e.stderr.decode() if e.stderr else e}")
        sys.exit(1)

    # Transcribe
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

    # Cleanup
    entries = cleanup(all_words, keep_backchannels=args.keep_backchannels)
    print(f"CLEANUP:final_entries={len(entries)}")

    # Format and write
    transcript = format_hyprnote(entries, session_id)
    with open(out_path, "w") as f:
        json.dump(transcript, f, indent=2)

    print(f"OUTPUT_FILE:{out_path}")


if __name__ == "__main__":
    main()
