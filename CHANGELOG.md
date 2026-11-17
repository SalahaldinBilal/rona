# Changelog

## [Unreleased]

### Changed
- The remembered volume levels and the window's place and size are saved to `state.conf` instead of `settings.conf`, so the settings file only changes when you change a setting. Values saved by earlier versions carry over.

### Fixed
- A file you open now starts playing, even if the previous one was paused or the file was paused when you last left it.

## [0.1.1] - 2026-11-17

### Added
- On Windows the installer registers Rona as a video and audio player. It shows up under Open with and in Settings > Apps > Default apps, without taking any file type from the app that has it, and uninstalling removes it again.
- "Resume audio where I left off" setting, off by default. Resuming audio and video are now separate options, and audio starts from the beginning unless this one is on.
- `--new-instance` starts a separate window even while Rona is already open.

### Changed
- The volume you set in the player is remembered, one level for audio files and one for videos, and each file starts at its kind's level. This replaces the Start volume setting, and a saved start volume becomes the starting level for both.
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