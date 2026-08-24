# Vendored crossterm — provenance and upgrade path

This directory vendors **crossterm 0.28.1** (crates.io release, MIT license)
and is wired workspace-wide through `[patch.crates-io]` in the root
`Cargo.toml`. The `examples/` directory and its `[[example]]` manifest targets
were dropped; a `[workspace]` marker was appended so the crate builds
standalone inside this repository.

## Local patch (the only reason this vendor exists)

Mode 2031 color-scheme change support, needed for live auto-theming
(including across SSH):

1. `src/event.rs`
   - `Event::ThemeModeChanged(ThemeMode)` variant and the `ThemeMode`
     enum (`Dark`/`Light`).
   - Commands `EnableThemeModeUpdates` (`CSI ? 2031 h`),
     `DisableThemeModeUpdates` (`CSI ? 2031 l`), and `RequestThemeMode`
     (`CSI ? 996 n`).
2. `src/event/sys/unix/parse.rs`
   - The `CSI ? … n` final byte now terminates the sequence:
     `? 997 ; 1|2 n` parses into `ThemeModeChanged`, any other private
     DSR report clears the parser buffer via `Err`. Unpatched crossterm
     returned `Ok(None)` forever for these reports, wedging the buffer
     and swallowing every later input event.
   - Parser unit tests for both report values, the incomplete prefix,
     and the buffer-clearing behavior of unknown reports.

Everything else is byte-identical to the upstream 0.28.1 release.

## Upgrade path

- **Preferred**: if a future crossterm release ships native mode 2031 /
  theme-mode events (track upstream issues on color-scheme queries),
  delete this directory, drop the `[patch.crates-io]` entry, bump the
  workspace `crossterm` version, and migrate the pager's
  `ThemeModeChanged` match arms and the three commands to the upstream
  API names.
- **Re-vendor**: to move to a newer upstream without native support,
  copy the new release from the cargo registry cache, re-apply the two
  files' patches above (both are small and localized), re-append the
  `[workspace]` marker, strip `[[example]]` targets, and run
  `cargo test --lib` inside this directory (delete the generated
  `target/` and `Cargo.lock` afterwards; they must not be committed).
- After either change, refresh the root `Cargo.lock`
  (`cargo metadata` once) and run the pager suite.
