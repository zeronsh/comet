# Workspace image preview and zoom

Files opens PNG, JPEG (`jpg`/`jpeg`), GIF, WebP, SVG, BMP and TIFF (`tif`/`tiff`) in a read-only image surface. Extension matching is case-insensitive. Loading, transfer/decode errors and size or memory limits are displayed in the surface. Other files retain the existing text or unsupported/binary preview. Images do not create editor buffers, syntax highlights or autosave work.

Images initially fit the available area, preserve their aspect ratio and never upscale a small image automatically. Images opened from Files stay in the file panel when clicked; zoom and pan operate directly in that panel. Markdown/diagram images and attachments can still open the shared lightbox. Files, Markdown/diagram lightboxes and attachment lightboxes share these gestures:

- Pinch on the trackpad or hold **Ctrl** while scrolling the mouse wheel to zoom around the pointer.
- Drag the image, or scroll without Ctrl, to pan when the image exceeds the viewport.
- The initial fit responds to viewport resizing. Manual zoom retains its scale and clamps the pan to the new viewport.
- Escape or a plain lightbox click closes it and restores focus. Dragging, including releasing on the scrim, does not close it.

Zoom normally spans 1%–3200%, allowing a smaller minimum when needed to fit a large image. Layout dimensions are capped at 131072 logical pixels for extreme SVGs. Scaling and panning reuse the current texture instead of decoding or rasterizing on each gesture. Gestures notify the owning view rather than refreshing the whole window, preserving unrelated view caches. Ctrl + wheel up increases zoom; wheel down decreases it. SVG detail therefore remains bounded by the raster budget at high zoom.

## Routing and resource lifecycle

`files/image_preview.rs` reads image bytes exclusively through `WorkspaceFilesClient::read_image` (`ReadWorkspaceImage`). The request retains its workspace target and target device. It requires a nonempty expected checkout ID. For legacy chats/plain folders with no synced checkout identity, `ReadWorkspaceFile` first obtains the owning host's checkout ID; its text is discarded and never becomes the image or an editor buffer. The UI never opens the workspace path on its own filesystem.

The existing transport limits remain: at most 8 MiB of image bytes, 384 KiB chunks, stable checkout ID, content hash, MIME type and size across chunks, valid base64, monotonic offsets and consistent completion. Empty checkout identities and content hashes are rejected. Decoding runs on the background executor, with a 30-second deadline for the complete read/decode request.

`image_media.rs` is shared with Markdown. Raster decoding limits each side to 4096 pixels and decoder allocation to 64 MiB. Animated input is flattened to its first frame. SVG parsing disables embedded/external image resolution and reserializes the parsed tree, so scripts, HTML and resource URLs do not reach GPUI. Prepared SVG rasters are capped at 4096 pixels per side; enlarged variants at 2097152 pixels. Memory accounting includes the prepared source, decoded CPU pixels and GPU texture.

Each Files image surface admits at most 64 MiB of retained image resources, including its panel rendering variants. A generation guard rejects obsolete work. Collapsing the right panel or switching files, tabs or chats suspends image loads, clears media and schedules asset/atlas eviction. Resuming reloads from the owning workspace. Watcher updates do not reload hidden images; modifications, deletion and renaming invalidate the relevant image. Changing the target suspends the old view before clearing documents. Closing/disposal releases the image preview resources. Text documents retain their existing cache and editing lifecycle. Renaming an edited text file to an image extension preserves its buffer and pending save; image conversion waits until those edits are resolved. Watcher events continue through text conflict handling while edits remain.

## Verification

Automated coverage includes image format selection, owner-routed loading with and without synced checkout metadata, malformed/repeated/inconsistent/oversized chunks, bounded decoding, SVG sanitization, animation flattening, stale completion rejection, cancellation, disposal, and zoom geometry. A headless GPUI test dispatches actual wheel, pinch, drag, click and Escape events through the rendered lightbox. A rendered Files test loads through the owning-device client and verifies that clicking the image retains panel focus and that subsequent zoom still targets the file panel. Existing Markdown lightbox tests also verify opening and focus restoration. Regression coverage checks edited text renamed to an image extension across reopen, watcher and pending-save paths; suspension during panel collapse; wheel direction; and reuse of an unrelated cached view during zoom.

The `workspace_file_surface_proxies_over_the_relay` integration test runs two engines through a relay. It reads an image on the owning engine, rejects an un-routed request and a wrong checkout, reconstructs a multi-chunk image, then rejects a continuation after a same-length content change.

Validation commands:

```sh
cargo test --release --locked -p zeron-ui --lib -- --test-threads=1
cargo test --release --locked -p zeron-engine --lib --test workspace_files --test device_routing
```

Automated results recorded on 2026-09-12 (Linux):

| Suite | Result |
| --- | --- |
| UI library, including rendered image/lightbox regressions | 851 passed |
| Engine library | 177 passed |
| Device routing integration | 7 passed |
| Workspace files integration | 3 passed |
| Formatting of changed Rust files and whitespace checks | Passed |

Physical-device validation is separate from these automated tests. This implementation session runs on Linux and does not establish real trackpad feel or macOS GPU behavior. Manual checks still to record:

- macOS trackpad: pinch in/out at an off-center point, successive gestures and dragging; Escape restores focus.
- Linux Wayland and X11 trackpads: the same checks, on a compositor/server that exposes native pinch events.
- Physical mouse: Ctrl + wheel zooms; unmodified wheel pans; releasing a drag outside the image does not close the lightbox.
- A second physical remote device: open images, switch files during loading, modify/rename/delete them, and switch workspace/device while requests are pending.

No zui dependency changes are required. Zeron uses `on_pinch` and `on_scroll_wheel` from its existing pinned revision.
