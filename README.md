# Comet

Control your coding agents (Claude Code, Codex, Grok, Hermes, Pi) from any of
your devices.

![Comet running a Claude Code session](docs/screenshot.png)

Every device runs a small engine that keeps your sessions in sync: start an
agent on one machine, follow and drive it from another. Install the engine as
a daemon on an always-on machine (a VPS, a spare box) and your agents keep
working after you close your laptop.

## Install the daemon (Linux)

```bash
curl -fsSL https://comet.zeron.sh/install.sh | sh
comet login                          # sign in (paste a code, done)
systemctl --user start comet-native
```

No configuration needed. Day-to-day:

```bash
comet status      # signed in? engine running?
comet update      # update to the latest release
comet daemon start|stop|restart|status
```

On macOS: build `comet` from source, then `comet daemon install` (launchd).

---

Developing or curious how it works? See [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
