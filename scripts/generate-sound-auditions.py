#!/usr/bin/env python3
"""Ten original auditions using Zeron's approved rounded pressure-pulse click.
Standalone standard-library synthesis. Only --install-attention changes an
application asset.
"""
from pathlib import Path
import math
import struct
import wave
import json
import argparse

RATE = 48000
MASTER = .55
# Harmonics are quiet, harmonic and rapidly damped; no noise or metallic modes.
COLORS = {
    'felt': [(1, 1, 1), (2, .075, .55), (3, .018, .35)],
    'warm': [(1, 1, 1), (2, .16, .60)],
    'clear': [(1, 1, 1), (2, .10, .55), (3, .035, .32)],
    'hollow': [(1, 1, 1), (3, .09, .40)],
}
# Clicks: onset, amplitude, width. Notes: onset, Hz, amplitude, decay, color.
CUES = [
('01-send', 'Send accepted', 'One compact click with a tiny muted note.', .30,
 [( .009,.20,.00062)], [(.040,293.66,.036,.045,'felt')]),
('02-queued', 'Added to queue', 'Three diminishing taps, ending with a low warm hint.', .48,
 [(.009,.20,.00065),(.099,.16,.00075),(.229,.11,.00090)],
 [(.255,261.63,.030,.060,'warm')]),
('03-upload-ready', 'Upload ready', 'Two clicks open a small ascending three-note flourish.', .74,
 [(.009,.15,.00080),(.099,.20,.00058)],
 [(.135,261.63,.043,.070,'clear'),(.220,329.63,.038,.080,'clear'),(.305,392,.040,.105,'clear')]),
('04-complete', 'Completion — resolve', 'A round pair with a descending, settled chime.', .73,
 [(.009,.17,.00055),(.149,.21,.00088)],
 [(.185,392,.037,.080,'felt'),(.305,293.66,.050,.120,'felt')]),
('05-question', 'Question — open interval', 'A patient pair followed by two gently rising notes.', .79,
 [(.009,.16,.00090),(.219,.20,.00058)],
 [(.250,329.63,.038,.085,'hollow'),(.365,440,.035,.120,'hollow')]),
('06-attention', 'Needs attention', 'Two broader taps and a restrained downward step.', .65,
 [(.009,.19,.00095),(.189,.17,.00082)],
 [(.225,349.23,.043,.085,'warm'),(.335,311.13,.039,.100,'warm')]),
('07-mic-on', 'Microphone on', 'A light opening pair with a quick upward fifth.', .55,
 [(.009,.14,.00082),(.089,.18,.00054)],
 [(.120,261.63,.035,.055,'clear'),(.215,392,.038,.075,'clear')]),
('08-mic-off', 'Microphone off', 'The closing counterpart: broader click and downward fifth.', .55,
 [(.009,.18,.00054),(.109,.14,.00082)],
 [(.135,392,.032,.050,'felt'),(.225,261.63,.038,.075,'felt')]),
('09-reconnected', 'Connection restored', 'Three growing taps land on a soft two-note harmony.', .76,
 [(.009,.11,.00090),(.139,.15,.00074),(.249,.20,.00060)],
 [(.280,293.66,.041,.120,'warm'),(.290,369.99,.029,.110,'felt')]),
('10-undo', 'Undo / restore', 'Two reversed-weight clicks with a brief falling chime.', .49,
 [(.009,.20,.00055),(.099,.12,.00088)],
 [(.125,329.63,.031,.055,'hollow'),(.200,293.66,.037,.065,'hollow')]),
]


def render(spec):
    name, label, description, duration, clicks, notes = spec
    count = round(duration * RATE)
    dry = []
    for i in range(count):
        t = i / RATE
        click = 0.0
        for center, amplitude, width in clicks:
            x = (t - center) / width
            if abs(x) < 5:
                click += amplitude * (1 - 2*x*x) * math.exp(-x*x)
        tone = 0.0
        for start, frequency, amplitude, decay, color in notes:
            u = t - start
            if u < 0:
                continue
            for ratio, level, damping in COLORS[color]:
                envelope = (1 - math.exp(-u/.010)) * math.exp(-u/(decay*damping))
                tone += amplitude * level * envelope * math.sin(2*math.pi*frequency*ratio*u)
        dry.append((click,tone))
    frames=[]
    for i,(click,tone) in enumerate(dry):
        channels=[]
        for delay in [.026,.035]:
            k=i-round(delay*RATE)
            reflection=.045*dry[k][1] if k>=0 else 0
            fade=min(1,(count-i)/(.060*RATE))
            channels.append((click+tone+reflection)*MASTER*fade)
        frames.append(channels)
    peak=max(abs(v) for frame in frames for v in frame)
    assert peak < .13
    return frames, peak


def write(path, frames):
    with wave.open(str(path),'wb') as f:
        f.setnchannels(2); f.setsampwidth(2); f.setframerate(RATE)
        f.writeframes(b''.join(struct.pack('<hh',*(round(v*32767) for v in frame)) for frame in frames))


if __name__=='__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, default=Path(__file__).resolve().parents[1] / 'docs/sound-design/auditions')
    parser.add_argument(
        '--install-attention',
        action='store_true',
        help='Also install the selected attention cue as the application asset.',
    )
    args = parser.parse_args()
    folder = args.output_dir
    folder.mkdir(parents=True, exist_ok=True)
    manifest=[]
    for cue in CUES:
        frames,peak=render(cue)
        write(folder/(cue[0]+'.wav'),frames)
        if args.install_attention and cue[0] == '06-attention':
            asset = Path(__file__).resolve().parents[1] / 'crates/ui/assets/sounds/attention.wav'
            write(asset, frames)
        manifest.append(dict(file=cue[0]+'.wav',name=cue[1],description=cue[2],duration=cue[3],
                             peak_dbfs=round(20*math.log10(peak),1)))
    (folder/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    rows=['# Rounded sound family — ten additional auditions\n',
          'Original synthesis using the approved rounded clicks, with wider rhythmic and tonal variety. Quiet levels, no noisy tails or metallic impacts.\n',
          'The numbered files remain audition references. `06-attention.wav` is also the source for the selected application attention cue.\n',
          '| # | Intended action | Character | Duration | Peak |',
          '|---|---|---|---:|---:|']
    for n,item in enumerate(manifest,1):
        rows.append(f"| {n:02} | [{item['name']}]({item['file']}) | {item['description']} | {item['duration']:.2f}s | {item['peak_dbfs']:.1f} dBFS |")
    rows += ['\nRegenerate with `python3 scripts/generate-sound-auditions.py`; pass `--install-attention` to refresh the selected runtime copy. Stereo 48 kHz, 16-bit PCM. No external samples. Perceived loudness depends on playback; no upward normalization is applied.',
             '\nThe action names are audition contexts, not recommendations to enable all ten. Frequent actions such as Send and Undo stay silent; the product sound scope is intentionally limited to completion, input required, errors or durable disconnections, and Appshot capture.']
    (folder/'README.md').write_text('\n'.join(rows)+'\n')
    print(json.dumps(manifest,indent=2))
