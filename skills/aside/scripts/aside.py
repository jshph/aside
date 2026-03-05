#!/usr/bin/env python3
"""Aside audio processing toolkit.

Subcommands:
    transcribe  Transcribe stereo WAV into Hyprnote transcript.json format.
    diarize     Diarize + transcribe mono/mixed audio (in-person, phone, voice memo).
    align       Align memo + transcript into timeline markdown.

Usage:
    python3 aside.py transcribe <wav_path> [--output <path>] [--keep-backchannels] [--model <path>]
    python3 aside.py diarize <audio_file> [--output <path>] [--num-speakers N] [--chunk-secs N] [--keep-backchannels] [--model <path>]
    python3 aside.py align --memo <path> --transcripts <path>... --meta <path> --output <path>

Stdout protocol (machine-parseable lines):
    SPLITTING_CHANNELS          splitting stereo into mono
    TRANSCRIBING:0 / :1        transcribing each channel
    CONVERTING                  converting to 16kHz mono WAV
    DIARIZING:chunk=N,offset=M  diarizing chunk N
    DIARIZE:segments=N,speakers=M  diarization complete
    TRANSCRIBING                whisper transcription (full file)
    MERGE:words=N,filtered=M   merge results
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


def convert_to_wav_16k(input_path: str) -> tuple[str, bool]:
    """Convert audio to 16kHz mono PCM WAV if needed.

    Returns (wav_path, is_temp) — is_temp is True if a temp file was created.
    """
    input_path = os.path.expanduser(input_path)
    if not os.path.isfile(input_path):
        raise FileNotFoundError(f"File not found: {input_path}")

    # Probe format
    try:
        probe = subprocess.run(
            [
                "ffprobe", "-v", "error",
                "-select_streams", "a:0",
                "-show_entries", "stream=sample_rate,channels,codec_name",
                "-of", "json",
                input_path,
            ],
            capture_output=True, text=True, check=True,
        )
        info = json.loads(probe.stdout)
        stream = info.get("streams", [{}])[0]
        rate = int(stream.get("sample_rate", 0))
        channels = int(stream.get("channels", 0))
        codec = stream.get("codec_name", "")
    except (subprocess.CalledProcessError, json.JSONDecodeError, ValueError):
        rate, channels, codec = 0, 0, ""

    if rate == 16000 and channels == 1 and codec == "pcm_s16le":
        return input_path, False

    wav_path = tempfile.mktemp(suffix="_16k.wav")
    subprocess.run(
        [
            "ffmpeg", "-y", "-i", input_path,
            "-ar", "16000", "-ac", "1", "-acodec", "pcm_s16le",
            wav_path,
        ],
        check=True, capture_output=True,
    )
    return wav_path, True


def chunk_audio(wav_path: str, chunk_secs: int = 1800) -> list[tuple[str, float]]:
    """Split audio into chunks via ffmpeg.

    Returns [(chunk_path, offset_secs)]. Skips chunking if file is shorter
    than chunk_secs * 1.2.
    """
    duration = get_duration(wav_path)
    if duration < chunk_secs * 1.2:
        return [(wav_path, 0.0)]

    chunks = []
    offset = 0.0
    idx = 0
    while offset < duration:
        chunk_path = tempfile.mktemp(suffix=f"_chunk{idx}.wav")
        subprocess.run(
            [
                "ffmpeg", "-y", "-i", wav_path,
                "-ss", str(offset), "-t", str(chunk_secs),
                "-ar", "16000", "-ac", "1", "-acodec", "pcm_s16le",
                chunk_path,
            ],
            check=True, capture_output=True,
        )
        chunks.append((chunk_path, offset))
        offset += chunk_secs
        idx += 1
    return chunks


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


def _run_whisper(wav_path: str, model_path: str,
                 offset_ms: int = 0) -> list[dict]:
    """Run whisper-cli on a WAV and return parsed tokens.

    Each token dict has start_ms, end_ms, text (with offset applied).
    """
    out_prefix = tempfile.mktemp(suffix="_whisper")

    try:
        subprocess.run(
            [
                "whisper-cli",
                "-m", model_path,
                "-f", wav_path,
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
                "start_ms": offsets.get("from", 0) + offset_ms,
                "end_ms": offsets.get("to", 0) + offset_ms,
                "text": text,
            })
    return words


def transcribe_channel(audio_path: str, channel: int, model: str) -> list[dict]:
    """Transcribe a mono WAV via whisper-cli and return word dicts."""
    model_path = _resolve_model_path(model)
    words = _run_whisper(audio_path, model_path)
    for w in words:
        w["channel"] = channel
        w["id"] = str(uuid.uuid4())
    return words


# ---------------------------------------------------------------------------
# Diarization helpers (imports deferred to avoid cost for transcribe/align)
# ---------------------------------------------------------------------------


def _compute_centroids(embeddings, labels):
    """Mean embedding per speaker label.

    Args:
        embeddings: (N, D) numpy array of speaker embeddings.
        labels: (N,) numpy array of integer cluster labels.

    Returns:
        dict mapping label -> centroid vector (1-D numpy array).
    """
    import numpy as np

    centroids = {}
    for label in set(labels):
        mask = labels == label
        centroids[label] = np.mean(embeddings[mask], axis=0)
    return centroids


def _match_centroids(anchor, chunk):
    """Greedy max-cosine-similarity matching of chunk centroids to anchor.

    Args:
        anchor: dict {label: centroid_vector} from chunk 0.
        chunk: dict {label: centroid_vector} from current chunk.

    Returns:
        dict mapping chunk_label -> anchor_label.
    """
    from sklearn.metrics.pairwise import cosine_similarity
    import numpy as np

    anchor_labels = list(anchor.keys())
    chunk_labels = list(chunk.keys())

    anchor_matrix = np.array([anchor[l] for l in anchor_labels])
    chunk_matrix = np.array([chunk[l] for l in chunk_labels])

    sim = cosine_similarity(chunk_matrix, anchor_matrix)  # (C, A)

    mapping = {}
    used_anchor = set()
    # Greedy: pick highest similarity pair, remove both, repeat
    for _ in range(min(len(chunk_labels), len(anchor_labels))):
        best_val = -2.0
        best_ci, best_ai = 0, 0
        for ci in range(len(chunk_labels)):
            if chunk_labels[ci] in mapping:
                continue
            for ai in range(len(anchor_labels)):
                if anchor_labels[ai] in used_anchor:
                    continue
                if sim[ci, ai] > best_val:
                    best_val = sim[ci, ai]
                    best_ci, best_ai = ci, ai
        mapping[chunk_labels[best_ci]] = anchor_labels[best_ai]
        used_anchor.add(anchor_labels[best_ai])

    # Any unmatched chunk labels get mapped to themselves
    for cl in chunk_labels:
        if cl not in mapping:
            mapping[cl] = cl
    return mapping


def diarize_chunked(wav_path: str, num_speakers: int = 2,
                    chunk_secs: int = 1800) -> tuple[list, list]:
    """Diarize audio with cross-chunk speaker consistency.

    Args:
        wav_path: Path to 16kHz mono WAV.
        num_speakers: Expected speaker count (0 for auto).
        chunk_secs: Chunk duration in seconds.

    Returns:
        (diar_segments, vad_segments) where diar_segments are
        diarize.Segment objects and vad_segments are SpeechSegment objects.
    """
    from diarize import (
        Segment, SpeechSegment,
        run_vad, extract_embeddings, cluster_speakers,
        _build_diarization_segments, diarize as diarize_full,
    )
    import numpy as np

    chunks = chunk_audio(wav_path, chunk_secs)
    owns_chunks = len(chunks) > 1

    all_diar_segments = []
    all_vad_segments = []
    anchor_centroids = None

    ns = num_speakers if num_speakers > 0 else None

    try:
        for i, (chunk_path, offset) in enumerate(chunks):
            print(f"DIARIZING:chunk={i},offset={offset:.0f}", flush=True)

            if i == 0 and not owns_chunks:
                # Single file — use the high-level API directly
                result = diarize_full(chunk_path, num_speakers=ns)
                all_diar_segments.extend(result.segments)
                vad = run_vad(chunk_path)
                all_vad_segments.extend(vad)
                continue

            # Multi-chunk path: manual pipeline
            vad = run_vad(chunk_path)
            offset_vad = [
                SpeechSegment(start=seg.start + offset, end=seg.end + offset)
                for seg in vad
            ]
            all_vad_segments.extend(offset_vad)

            if not vad:
                continue

            embeddings, subsegments = extract_embeddings(chunk_path, vad)
            if len(embeddings) == 0:
                continue

            labels, _ = cluster_speakers(embeddings, num_speakers=ns)

            if i == 0:
                anchor_centroids = _compute_centroids(embeddings, labels)
            elif anchor_centroids is not None:
                chunk_centroids = _compute_centroids(embeddings, labels)
                label_map = _match_centroids(anchor_centroids, chunk_centroids)
                labels = np.array([label_map.get(l, l) for l in labels])

            segments = _build_diarization_segments(vad, subsegments, labels)
            for seg in segments:
                all_diar_segments.append(Segment(
                    start=seg.start + offset,
                    end=seg.end + offset,
                    speaker=seg.speaker,
                ))
    finally:
        _cleanup_chunks(chunks, owns_chunks)

    speakers = set(s.speaker for s in all_diar_segments)
    print(f"DIARIZE:segments={len(all_diar_segments)},speakers={len(speakers)}", flush=True)

    return all_diar_segments, all_vad_segments


def _cleanup_chunks(chunks: list[tuple[str, float]], owns_chunks: bool):
    """Remove temp chunk files if we created them."""
    if owns_chunks:
        for chunk_path, _ in chunks:
            try:
                os.unlink(chunk_path)
            except OSError:
                pass


def transcribe_full(wav_path: str, model: str,
                    chunk_secs: int = 1800) -> list[dict]:
    """Transcribe an audio file via whisper-cli, chunking if needed.

    Returns raw whisper JSON tokens as dicts with start_ms, end_ms, text.
    """
    model_path = _resolve_model_path(model)
    chunks = chunk_audio(wav_path, chunk_secs)
    owns_chunks = len(chunks) > 1

    all_words = []
    try:
        for i, (chunk_path, offset) in enumerate(chunks):
            print(f"TRANSCRIBING:chunk={i},offset={offset:.0f}", flush=True)
            offset_ms = int(offset * 1000)
            words = _run_whisper(chunk_path, model_path, offset_ms)
            all_words.extend(words)
    finally:
        _cleanup_chunks(chunks, owns_chunks)

    return all_words


def merge_diarization_with_whisper(whisper_words: list[dict],
                                   diar_segments: list,
                                   vad_segments: list) -> list[dict]:
    """Assign speaker channels to whisper tokens using diarization.

    Uses bisect for O(log n) lookup. Tokens outside all VAD segments are
    dropped (catches whisper silence hallucinations).

    Args:
        whisper_words: list of {start_ms, end_ms, text} from transcribe_full.
        diar_segments: Segment objects with start, end, speaker.
        vad_segments: SpeechSegment objects with start, end.

    Returns:
        list of word dicts with channel, start_ms, end_ms, text, id.
    """
    import bisect

    # Build sorted arrays for bisect
    diar_sorted = sorted(diar_segments, key=lambda s: s.start)
    diar_starts = [s.start for s in diar_sorted]

    vad_sorted = sorted(vad_segments, key=lambda s: s.start)
    vad_starts = [s.start for s in vad_sorted]

    # Map speaker labels to channel numbers (stable ordering)
    speaker_labels = sorted(set(s.speaker for s in diar_sorted))
    speaker_to_channel = {spk: i for i, spk in enumerate(speaker_labels)}

    words = []
    filtered = 0

    for tok in whisper_words:
        mid_s = (tok["start_ms"] + tok["end_ms"]) / 2000.0

        # VAD filter: drop tokens outside all speech segments
        vi = bisect.bisect_right(vad_starts, mid_s) - 1
        in_vad = False
        for check in range(max(0, vi), min(vi + 2, len(vad_sorted))):
            seg = vad_sorted[check]
            if seg.start <= mid_s <= seg.end:
                in_vad = True
                break
        if not in_vad:
            filtered += 1
            continue

        # Find diarization segment via bisect
        di = bisect.bisect_right(diar_starts, mid_s) - 1
        speaker = None

        # Check nearby segments for best match
        best_dist = float("inf")
        for check in range(max(0, di), min(di + 2, len(diar_sorted))):
            seg = diar_sorted[check]
            if seg.start <= mid_s <= seg.end:
                speaker = seg.speaker
                best_dist = 0
                break
            seg_mid = (seg.start + seg.end) / 2
            dist = abs(mid_s - seg_mid)
            if dist < best_dist:
                best_dist = dist
                speaker = seg.speaker

        # Fallback: nearest segment by midpoint
        if speaker is None and diar_sorted:
            speaker = min(diar_sorted,
                          key=lambda s: abs(mid_s - (s.start + s.end) / 2)).speaker

        channel = speaker_to_channel.get(speaker, 0)

        words.append({
            "channel": channel,
            "start_ms": tok["start_ms"],
            "end_ms": tok["end_ms"],
            "id": str(uuid.uuid4()),
            "text": tok["text"],
        })

    print(f"MERGE:words={len(words)},filtered={filtered}", flush=True)
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
    channels = sorted(set(w["channel"] for w in words))
    for ch in channels:
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
    channels = sorted(set(e["channel"] for e in cleaned)) if cleaned else []
    for ch in channels:
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


def cmd_diarize(args):
    """Execute the diarize subcommand."""
    input_path = os.path.expanduser(args.input)
    if not os.path.isfile(input_path):
        print(f"ERROR:File not found: {input_path}")
        sys.exit(1)

    out_path = args.output
    if not out_path:
        out_path = os.path.splitext(input_path)[0] + "_transcript.json"

    try:
        duration = get_duration(input_path)
        print(f"DURATION:{duration:.1f}", flush=True)
    except Exception:
        duration = None

    # Convert to 16kHz mono WAV
    print("CONVERTING", flush=True)
    try:
        wav_path, is_temp = convert_to_wav_16k(input_path)
    except subprocess.CalledProcessError as e:
        print(f"ERROR:ffmpeg conversion failed: {e.stderr.decode() if e.stderr else e}")
        sys.exit(1)

    diar_tmp = tempfile.mktemp(suffix="_diar.json")

    try:
        # Run diarization in a subprocess so native model memory is fully
        # reclaimed before whisper starts (onnxruntime + WeSpeaker hold
        # ~1GB that gc.collect() cannot free).
        num_speakers = args.num_speakers if args.num_speakers > 0 else 0
        diar_script = (
            f"import json, sys; sys.path.insert(0, {os.path.dirname(__file__)!r}); "
            f"from aside import diarize_chunked; "
            f"diar, vad = diarize_chunked({wav_path!r}, "
            f"num_speakers={num_speakers}, chunk_secs={args.chunk_secs}); "
            f"f = open({diar_tmp!r}, 'w'); "
            f"json.dump({{'diar': [{{'start': s.start, 'end': s.end, 'speaker': s.speaker}} for s in diar], "
            f"'vad': [{{'start': s.start, 'end': s.end}} for s in vad]}}, f); "
            f"f.close()"
        )
        result = subprocess.run(
            [sys.executable, "-c", diar_script],
            check=True, text=True,
        )

        # Load serialized diarization results
        with open(diar_tmp) as f:
            diar_data = json.load(f)

        # Transcribe (chunked to limit memory)
        whisper_words = transcribe_full(wav_path, args.model,
                                        chunk_secs=args.chunk_secs)

        # Build lightweight segment objects for merge
        from types import SimpleNamespace
        diar_segments = [SimpleNamespace(**s) for s in diar_data["diar"]]
        vad_segments = [SimpleNamespace(**s) for s in diar_data["vad"]]

        # Merge
        all_words = merge_diarization_with_whisper(
            whisper_words, diar_segments, vad_segments,
        )
    except subprocess.CalledProcessError as e:
        print(f"ERROR:Diarization failed (exit {e.returncode})")
        sys.exit(1)
    except Exception as e:
        print(f"ERROR:Diarization/transcription failed: {e}")
        sys.exit(1)
    finally:
        if is_temp:
            try:
                os.unlink(wav_path)
            except OSError:
                pass
        try:
            os.unlink(diar_tmp)
        except OSError:
            pass

    print(f"CLEANUP:raw_words={len(all_words)}", flush=True)
    entries = cleanup(all_words, keep_backchannels=args.keep_backchannels)
    print(f"CLEANUP:final_entries={len(entries)}", flush=True)

    transcript = format_hyprnote(entries)
    with open(out_path, "w") as f:
        json.dump(transcript, f, indent=2)

    print(f"OUTPUT_FILE:{out_path}", flush=True)


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

    # diarize subcommand
    p_diarize = subparsers.add_parser(
        "diarize",
        help="Diarize + transcribe mono/mixed audio (in-person, phone, voice memo).",
    )
    p_diarize.add_argument("input", help="Path to audio file (any ffmpeg-readable format)")
    p_diarize.add_argument("--output", "-o", help="Output path (default: <input>_transcript.json)")
    p_diarize.add_argument("--num-speakers", type=int, default=2,
                           help="Expected number of speakers (0 for auto, default: 2)")
    p_diarize.add_argument("--chunk-secs", type=int, default=1800,
                           help="Chunk duration in seconds for long files (default: 1800)")
    p_diarize.add_argument("--keep-backchannels", action="store_true",
                           help="Skip backchannel/filler removal passes")
    p_diarize.add_argument("--model", default=DEFAULT_MODEL,
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
    elif args.command == "diarize":
        cmd_diarize(args)
    elif args.command == "align":
        cmd_align(args)
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
