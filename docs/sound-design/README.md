# Rounded notification sounds

This contribution selects a compact four-cue product signature. It replaces the
existing completion and agent-question sounds, adds one shared attention cue for
run failures and durable connection outages, and preserves the Appshot capture cue
from the separate Appshots contribution.

- [`done.wav`](../../crates/ui/assets/sounds/done.wav): completion, settling
  rounded pair.
- [`request.wav`](../../crates/ui/assets/sounds/request.wav): agent question,
  rising rounded pair.
- [`attention.wav`](../../crates/ui/assets/sounds/attention.wav): failure or
  durable disconnection, restrained downward rounded pair.
- [`generate-notification-sounds.py`](../../scripts/generate-notification-sounds.py):
  reproduces the completion and question assets;
  `--variants-dir <directory>` also exports the two click-only alternatives.

Settings → Notifications has a master session-sounds switch and independent
choices for completion, input required, and errors/disconnections. Appshot capture
keeps its independent **Capture sound** setting on the Appshots page.

## Additional auditions

[Ten additional variants](auditions/README.md) cover send, queue, upload ready,
completion, questions, attention, microphone on/off, reconnection and undo. Only
the attention audition is promoted to an application asset; the rest remain
references without triggers.

Earlier click-only alternatives: [completion](auditions/done-minimal.wav) and
[agent question](auditions/request-minimal.wav).

Run `python3 scripts/generate-sound-auditions.py --install-attention` to regenerate
the ten numbered WAVs, their manifest, and the selected attention asset. All
synthesis uses Python's standard library and original rounded pressure pulses,
with no external samples.
