#!/usr/bin/env python3
"""Original rounded notification family; standard-library-only stereo PCM.

The selected Done and Request cues replace existing assets without changing
notification triggers. Optional minimal alternatives are audition files only.
No external samples, noisy tails, resonant impacts or upward normalization.
"""
import argparse
import math
from pathlib import Path
import struct
import wave

RATE = 48000
MASTER_GAIN = 0.55
# (duration, [(center, amplitude, width)], [(start, frequency, amplitude, decay)])
CUES = {
    # A brighter tap settles into a broader, softer final click.
    'done': (0.52, [(0.009, .18, .00052), (.119, .21, .00085)],
             [(.145, 349.23, .055, .10)]),
    # The second tap is brighter and slightly stronger: a gentle invitation.
    'request': (.60, [(.009, .16, .00085), (.179, .21, .00052)],
                [(.205, 392.00, .05, .12)]),
    # Audition alternatives use only tactile clicks, without a tonal tail.
    'done-minimal': (.18, [(.009, .21, .00085)], []),
    'request-minimal': (.38, [(.009, .17, .00075), (.179, .21, .00052),
                              (.259, .12, .00060)], []),
}


def synthesize(cue):
    duration, clicks, tones = cue
    count = round(RATE * duration)
    dry = []
    for index in range(count):
        t = index / RATE
        click = 0.0
        for center, amplitude, width in clicks:
            position = (t - center) / width
            if abs(position) < 5:
                click += amplitude * (1 - 2 * position * position) * math.exp(-position * position)
        tone = 0.0
        for start, frequency, amplitude, decay in tones:
            u = t - start
            if u >= 0:
                envelope = (1 - math.exp(-u / .009)) * math.exp(-u / decay)
                tone += amplitude * envelope * (math.sin(2 * math.pi * frequency * u)
                        + .12 * math.sin(2 * math.pi * frequency * 2 * u))
        dry.append((click, tone))
    frames = []
    for index, (click, tone) in enumerate(dry):
        channels = []
        for delay in [.024, .032]:
            offset = index - round(delay * RATE)
            reflection = .06 * dry[offset][1] if offset >= 0 else 0.0
            fade = min((count - index) / (.075 * RATE), 1.0)
            channels.append((click + tone + reflection) * fade * MASTER_GAIN)
        frames.append(channels)
    peak = max(abs(value) for frame in frames for value in frame)
    assert peak < 0.14, 'Keep the entire family quiet without normalization.'
    return frames, peak


def write_cue(name, destination):
    frames, peak = synthesize(CUES[name])
    destination.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(destination), 'wb') as wav:
        wav.setnchannels(2)
        wav.setsampwidth(2)
        wav.setframerate(RATE)
        wav.writeframes(b''.join(struct.pack('<hh', *(round(v * 32767) for v in frame))
                                 for frame in frames))
    print(f'{destination}: {len(frames) / RATE:.2f}s, peak {20 * math.log10(peak):.1f} dBFS')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--variants-dir', type=Path, help='Also export all cues for audition.')
    args = parser.parse_args()
    assets = Path(__file__).resolve().parents[1] / 'crates/ui/assets/sounds'
    for name in ['done', 'request']:
        write_cue(name, assets / f'{name}.wav')
    if args.variants_dir:
        for name in CUES:
            write_cue(name, args.variants_dir / f'{name}.wav')
