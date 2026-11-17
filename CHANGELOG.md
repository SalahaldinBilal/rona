# Changelog

## [Unreleased]

### Added
- "Resume audio where I left off" setting, off by default. Resuming audio and video are now separate options, and audio starts from the beginning unless this one is on.
- `--new-instance` starts a separate window even while Rona is already open.

### Changed
- Opening a file while Rona is running plays it in the open window instead of starting a second one. Files opened together, like a selection in Explorer, become one playlist.
- When some of the files or links you open can't be played, a message says how many were skipped.

### Fixed
- Clicking the playing file in the playlist no longer restarts it.
- An audio file's cover no longer flashes at full size before shrinking to its place.
- Moving from one audio file to the next updates the cover and tags in place instead of blanking the view for a moment. Tags and covers of the files around the current one are read ahead, so the next file's info shows the moment it starts.
- Dragging files onto the window plays them again.
- Ctrl+V plays files copied in Explorer or a file manager, not only copied paths and links.
- Several files on the command line all go into the playlist, not just the first. Links given together with files are no longer dropped.
- A path that isn't valid Unicode no longer crashes Rona at startup. It shows File not found instead, since mpv only takes UTF-8 paths.

## [0.1.0] - 2026-11-17

- Initial Release