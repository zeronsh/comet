#!/usr/bin/env python3
"""Generate the original rounded Appshot confirmation cue.

Three separated shutter clicks followed by a single smooth C4-F4 resonance, adapted
from the rounded sound-family auditions. Standard-library-only, deterministic
stereo PCM; no external samples, noise bed or upward normalization.
"""
from pathlib import Path
import math
import struct
import wave

RATE = 48000
MASTER = .55
COLORS = {
    'felt': [(1, 1, 1), (2, .075, .55), (3, .018, .35)],
    'clear': [(1, 1, 1), (2, .10, .55), (3, .035, .32)],
}
# Clicks: onset, amplitude, width. Notes: onset, Hz, amplitude, decay, color.
APPSHOT = (
    'appshot', 'Appshot captured', 'Round shutter triple and blended confirmation', .67,
    [(.009, .19, .00075), (.079, .21, .00058), (.149, .18, .00068)],
    [(.180, 261.63, .030, .120, 'felt'),
     (.180, 349.23, .038, .120, 'felt'),
     (.180, 523.25, .009, .085, 'clear')],
)


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
                envelope = (1 - math.exp(-u/.018)) * math.exp(-u/(decay*damping))
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


if __name__ == '__main__':
    frames, peak = render(APPSHOT)
    output = Path(__file__).resolve().parents[1] / 'crates/ui/assets/sounds/appshot.wav'
    write(output, frames)
    print(f'{output}: {len(frames) / RATE:.2f}s, stereo, peak {20 * math.log10(peak):.1f} dBFS')
