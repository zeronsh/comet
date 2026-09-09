# Projectless session screenshots

Real Linux desktop application captures taken on 2026-09-09 from this branch, built with `cargo build -p zeron --locked`.

The application ran in Xvfb with Openbox at a 1200 × 800 window size. A separate local engine used temporary data directories, two empty demo repositories, the mock harness, and a device renamed to “Demo workstation.” No personal session history is included.

- `project-selector.png`: the project selector opened with native mouse input, with the pointer over “Don't work in a project.”
- `restored-selection.png`: after choosing that option, closing the application, and starting it again with the same UI data directory. The reopened selector highlights the saved opt-out, the target chip reads “No project,” and checkout controls are absent.

Images were captured directly from the application window using ImageMagick `import`; they are unedited screenshots, not mockups. These files document the PR and are not bundled into the application.
