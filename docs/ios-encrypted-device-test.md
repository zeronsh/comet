# Test encrypted sync on a Mac and iPhone

This branch supports the phone's encrypted device RPC, recent-message tails,
and full tool output/diff sidecars. Pairing refreshes automatically while the
Encryption sheet is open. Desktop control uses the same fixed Noise XX profile
as the Rust client, with membership checks and no plaintext fallback.

After the recovery kit is confirmed, the desktop automatically copies existing
chat history into encrypted storage. Settings → Encryption shows progress and
offers retry if a source or referenced tool output is unavailable. Copies are
read back and verified before each chat is marked complete; interrupted work
resumes on restart. Original plaintext copies are retained, so this does not yet
erase plaintext history from the relay. History held only on an offline desktop
is copied when that desktop joins encryption and comes online. Keep desktops
online until their migrations finish before relying on phone-only access.
The production worker must be updated separately before these paths can work
against production.

## Start the branch locally

Keep the Mac and phone on the same trusted Wi-Fi network. Local dev accepts
synthetic identities; do not expose this dev server to the public internet.

From the repository root, start the edge in one terminal:

```sh
cd edge
npm ci
npx wrangler dev --local --ip 0.0.0.0 --port 27640 \
  --var AUTH_MODE:dev --persist-to .wrangler/mobile-device-test
```

Use the repository-pinned Wrangler version: newer 4.119 builds crashed during
local WebSocket testing. Keep this terminal running while testing the devices.

In another terminal, start a separate desktop profile and IPC port:

```sh
export ZERON_DATA_DIR="$HOME/Library/Application Support/ZeronMobileTest"
export ZERON_IPC_PORT=27655
export ZERON_EDGE_URL="http://$(scutil --get LocalHostName).local:27640"
export ZERON_EDGE_TOKEN=mobile-test@mobile-test
export ZERON_ORG_ID=mobile-test
cargo run -p zeron
```

Allow macOS incoming connections if prompted. The phone's edge URL must use
the Mac's `.local` hostname, never `localhost` (which would mean the phone).
Use exactly the same edge URL on both devices throughout this test; the
phone's local vault storage is scoped to the origin as well as the account.

On the desktop, open **Settings → Encryption**, create a vault, and save and
confirm the recovery kit. Copy its full vault fingerprint.

## Install and pair the phone

1. Connect and unlock the iPhone. Enable Developer Mode if Xcode requests it.
2. Open `apps/ios/Zeron.xcodeproj`, choose the phone, and run the **Zeron**
   scheme with your existing signing team. Under **Edit Scheme → Run → Info**,
   choose the **Debug** build configuration.
3. Tap **Dev sign in** below **Log in to Zeron** (Debug builds only). Use the edge URL printed by
   `echo "http://$(scutil --get LocalHostName).local:27640"` on the Mac,
   user `mobile-test`, and organization `mobile-test`. Allow Local Network
   access when iOS asks.
4. Open **Encryption**, paste the desktop's fingerprint, and tap
   **Approve from another device**.
5. The running desktop checks for requests every five seconds and posts a
   notification for each new request, including reapproval. Click **Review**
   in the sidebar to open Encryption settings. System banners follow the
   desktop notification toggle and macOS notification permissions.
   On the desktop, approve the pending phone only after comparing the full
   eight-digit code on both screens. Keep the phone's Encryption sheet open;
   it should switch to **Encrypted** automatically.

The CLI equivalent for the isolated desktop's approval is:

```sh
ZERON_IPC_PORT=27655 target/debug/zeron vault requests
ZERON_IPC_PORT=27655 target/debug/zeron vault approve REQUEST_ID CODE_FROM_PHONE
```

## Exercise the real devices

In desktop **Settings → Encryption → Approved devices**, use **Rename** to
choose names such as Laptop or iPhone. Labels sync encrypted and remain
available for removed identities.

- Create a new space from the phone. Browse a desktop folder and create a
  chat; this exercises encrypted RPC, not just mirrored chat data.
- Send a prompt from each device and watch both transcripts converge.
- Attach an image from the phone and check that the desktop receives it.
- Run a tool with long output. Expand its activity row and choose **Show full
  output**; **Show full diff** appears when the host published a diff reference.
- Open a cold chat: its sealed recent-message tail can render while the full
  checkpoint and log load. A tail never advances the sync cursor.
- Background the phone, bring it back, and repeat folder browsing and sending.
- With the recovery kit saved, revoke the phone from the desktop. Remote
  control must stop. While active, the phone checks membership every five
  seconds and shows **Get approval to view your chats**. Tap **Ask for
  approval**, compare the large code with the desktop, and approve it.
  Recovery is available separately; connection details are behind the info
  button. Foregrounding also checks immediately. Verify the phone returns
  to the home screen automatically after approval.

A missing sidecar shows an error with Retry; the full transcript continues
loading through the checkpoint/log path. Sidecar reads do not require the
host desktop to remain online after it has published them.

## Repeatable automated check

With Xcode and the edge's npm dependencies installed:

```sh
scripts/test-ios-vault-live.sh
```

The script starts an isolated local worker on port 27641 and a Rust HostRelay,
creates a disposable vault, then runs the actual iOS clients on the simulator.
It tests enrollment, concurrent cold RPC calls, streaming replies, large
chunked requests/replies, sealed tails and blobs, reconnect, and revocation.
It also runs deterministic Snow/Swift conformance and sidecar rejection tests.
Automatic approval exists only in this disposable test host.

The Rust migration test uses the real local worker and covers a checkpoint
larger than the row size limit, plaintext checkpoint plus final rows, missing
tool output, restart and retry, recovery-kit reads, retained originals, and
preservation of message IDs and the processed-command ledger:

```sh
ZERON_VAULT_EDGE_URL=http://127.0.0.1:27640 cargo test -p zeron-engine --test vault_e2e plaintext_history_migrates
```

Logs are in `/tmp/comet-mobile-e2e`. Override the simulator with
`ZERON_IOS_TEST_DESTINATION='platform=iOS Simulator,name=iPhone 17 Pro Max'`.
The live test skips during normal test runs unless its companion host has
created a connection file.

To check or regenerate the deterministic Rust fixture:

```sh
cargo test -p zeron-crypto --test channel_fixture
UPDATE_CHANNEL_FIXTURE=1 cargo test -p zeron-crypto --test channel_fixture
```

Protocol references: [Noise specification](https://noiseprotocol.org/noise.html),
[Rust channel implementation](../crates/crypto/src/channel.rs), and
[Wrangler local development](https://developers.cloudflare.com/workers/wrangler/commands/dev/).
