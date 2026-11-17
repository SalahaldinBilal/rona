# Changelog

## [Unreleased]

## [0.1.2] - 2026-11-17

### Added
- Shuffle and repeat for playlists, in the playlist panel and the audio view. Shuffle picks a random play order without rearranging the list, and repeat plays the playlist once more or forever. Both are remembered separately for audio and video playlists.
- Audio files show their cover and artist in the playlist, also in playlists that mix audio and video.
- Playlist entries can be dragged into a new order, removed, played next or moved to the top or bottom from a right-click menu. A removal can be undone from the message that follows it or with Ctrl+Z.
- The audio view has a new look: a background in the cover's colors, the cover beside the details on wide windows, rounded corners, the track number, format tags such as bit depth and sample rate, the playing time and what plays next. Files without tags show their type and size.
- Theme page in Settings with theme profiles. A profile can change every color, the bar heights and the animation timings, and any value can be reverted to the default on its own. Colors have a picker with transparency, and the color field also takes pasted `rgb()`, `hsl()` and short hex colors. The default theme can't be changed, a new profile starts as a copy of the one shown. Profiles are saved as files in the `themes` folder next to `settings.conf`.
- V hides or shows subtitles without turning the track off. It can be changed under Shortcuts in Settings. "Show hidden subtitles again on the next file" (Subtitles settings, on by default) decides whether they stay hidden when the next file starts.

### Changed
- The start screen, loading and error screens and the audio view use a dark gray background instead of black, which is easier on the eyes and shows dark details. Only the bars around a playing video stay black.
- The remembered volume levels and the window's place and size are saved to `state.conf` instead of `settings.conf`, so the settings file only changes when you change a setting. Values saved by earlier versions carry over.

### Fixed
- "Connection lost" no longer pops up during playback of a link that plays fine. When the server drops the connection while the buffer is full, Rona reconnects right away and only shows the message if playback has to wait. It also keeps trying instead of stopping after three attempts.
- After a long stretch without downloading, "No data for N s" no longer counts that time and shows up right away on a seek.
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