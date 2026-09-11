# Rounded sound family — ten additional auditions

Original synthesis using the approved rounded clicks, with wider rhythmic and tonal variety. Quiet levels, no noisy tails or metallic impacts.

The numbered files remain audition references. `06-attention.wav` is also the source for the selected application attention cue.

| # | Intended action | Character | Duration | Peak |
|---|---|---|---:|---:|
| 01 | [Send accepted](01-send.wav) | One compact click with a tiny muted note. | 0.30s | -19.2 dBFS |
| 02 | [Added to queue](02-queued.wav) | Three diminishing taps, ending with a low warm hint. | 0.48s | -19.2 dBFS |
| 03 | [Upload ready](03-upload-ready.wav) | Two clicks open a small ascending three-note flourish. | 0.74s | -19.2 dBFS |
| 04 | [Completion — resolve](04-complete.wav) | A round pair with a descending, settled chime. | 0.73s | -18.7 dBFS |
| 05 | [Question — open interval](05-question.wav) | A patient pair followed by two gently rising notes. | 0.79s | -19.2 dBFS |
| 06 | [Needs attention](06-attention.wav) | Two broader taps and a restrained downward step. | 0.65s | -19.6 dBFS |
| 07 | [Microphone on](07-mic-on.wav) | A light opening pair with a quick upward fifth. | 0.55s | -20.1 dBFS |
| 08 | [Microphone off](08-mic-off.wav) | The closing counterpart: broader click and downward fifth. | 0.55s | -20.1 dBFS |
| 09 | [Connection restored](09-reconnected.wav) | Three growing taps land on a soft two-note harmony. | 0.76s | -19.2 dBFS |
| 10 | [Undo / restore](10-undo.wav) | Two reversed-weight clicks with a brief falling chime. | 0.49s | -19.2 dBFS |

Regenerate with `python3 scripts/generate-sound-auditions.py`; pass `--install-attention` to refresh the selected runtime copy. Stereo 48 kHz, 16-bit PCM. No external samples. Perceived loudness depends on playback; no upward normalization is applied.

The action names are audition contexts, not recommendations to enable all ten. Frequent actions such as Send and Undo stay silent; the product sound scope is intentionally limited to completion, input required, errors or durable disconnections, and Appshot capture.
