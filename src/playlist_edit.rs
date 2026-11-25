use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::images::{self, Size};
use crate::logging::{info, warn};
use crate::playlist::{self, with_playlist};
use crate::playlist_file::{self, Entry, Kind, Origin, PlaylistFile, RepeatMode, Series, Skip, Subtitle};
use crate::{AppWindow, HeaderItem, Osd, Player, PlaylistDoc, SectionItem, SkipItem, SubtitleItem, WindowControls, opener};

/// What waits on the save prompt.
#[derive(Clone, Copy, Debug)]
pub enum Then {
    Open,
    Close,
}

struct Edit {
    app: slint::Weak<AppWindow>,
    osd: Osd,
    headers: Rc<VecModel<HeaderItem>>,
    sections: Rc<VecModel<SectionItem>>,
    // The section each row of the dialog was before, None for one added there.
    section_origins: Vec<Option<usize>>,
    mirrors: Rc<VecModel<SharedString>>,
    entry_headers: Rc<VecModel<HeaderItem>>,
    skips: Rc<VecModel<SkipItem>>,
    subtitles: Rc<VecModel<SubtitleItem>>,
    entry_index: usize,
    // The entry as it was, so fields the dialog doesn't show survive a save.
    entry_base: Option<Entry>,
    then: Option<Then>,
}

thread_local! {
    static EDIT: RefCell<Option<Edit>> = const { RefCell::new(None) };
}

fn with_edit<T>(action: impl FnOnce(&mut Edit) -> T) -> Option<T> {
    EDIT.with(|slot| slot.borrow_mut().as_mut().map(action))
}

fn doc(app: &AppWindow) -> PlaylistDoc<'_> {
    app.global::<PlaylistDoc>()
}

fn with_app(action: impl FnOnce(&AppWindow)) {
    let app = EDIT.with(|slot| slot.borrow().as_ref().and_then(|edit| edit.app.upgrade()));
    if let Some(app) = app {
        action(&app);
    }
}

fn osd(text: impl Into<SharedString>) {
    let osd = EDIT.with(|slot| slot.borrow().as_ref().map(|edit| edit.osd.clone()));
    if let Some(osd) = osd {
        osd.show(text);
    }
}

pub fn wire(app: &AppWindow, osd: &Osd) {
    let edit = Edit {
        app: app.as_weak(),
        osd: osd.clone(),
        headers: Rc::new(VecModel::default()),
        sections: Rc::new(VecModel::default()),
        section_origins: Vec::new(),
        mirrors: Rc::new(VecModel::default()),
        entry_headers: Rc::new(VecModel::default()),
        skips: Rc::new(VecModel::default()),
        subtitles: Rc::new(VecModel::default()),
        entry_index: 0,
        entry_base: None,
        then: None,
    };
    let document = doc(app);
    document.set_d_headers(ModelRc::from(edit.headers.clone()));
    document.set_d_sections(ModelRc::from(edit.sections.clone()));
    document.set_e_mirrors(ModelRc::from(edit.mirrors.clone()));
    document.set_e_headers(ModelRc::from(edit.entry_headers.clone()));
    document.set_e_skips(ModelRc::from(edit.skips.clone()));
    document.set_e_subtitles(ModelRc::from(edit.subtitles.clone()));
    EDIT.with(|slot| *slot.borrow_mut() = Some(edit));

    document.on_save(|| save_or_ask(None));
    document.on_save_as(|| save_as(None));
    document.on_toggle_autosave(toggle_autosave);
    document.on_export_m3u(export_m3u);
    document.on_show_in_folder(show_in_folder);
    document.on_add_files(opener::add_files);
    document.on_add_link(opener::add_link);
    document.on_find_file(|index| find_file(index as usize));
    document.on_prompt_answer(prompt_answer);
    document.on_valid_time(|text| text.trim().is_empty() || playlist_file::parse_time(&text).is_some());

    document.on_open_details(open_details);
    document.on_details_save(details_save);
    document.on_details_cancel(|| with_app(|app| doc(app).set_details_open(false)));
    document.on_browse_image(browse_image);
    document.on_preview_cover(|_| refresh_previews());
    document.on_add_header(|| {
        with_edit(|edit| edit.headers.push(HeaderItem::default()));
    });
    document.on_remove_header(|index| {
        with_edit(|edit| remove_row(&edit.headers, index));
    });
    document.on_add_section(|| {
        with_edit(|edit| {
            edit.sections.push(SectionItem::default());
            edit.section_origins.push(None);
        });
    });
    document.on_remove_section(|index| {
        with_edit(|edit| {
            if (index as usize) < edit.section_origins.len() {
                edit.section_origins.remove(index as usize);
                remove_row(&edit.sections, index);
            }
        });
    });
    document.on_move_section(|index, step| {
        with_edit(|edit| {
            let (from, to) = (index as usize, (index + step) as usize);
            if index + step < 0 || to >= edit.section_origins.len() || from >= edit.section_origins.len() {
                return;
            }
            edit.section_origins.swap(from, to);
            let (a, b) = (edit.sections.row_data(from), edit.sections.row_data(to));
            if let (Some(a), Some(b)) = (a, b) {
                edit.sections.set_row_data(from, b);
                edit.sections.set_row_data(to, a);
            }
        });
    });

    document.on_edit_entry(|index| open_entry(index as usize));
    document.on_entry_save(|| {
        entry_save();
        with_app(|app| doc(app).set_entry_open(false));
    });
    document.on_entry_cancel(|| with_app(|app| doc(app).set_entry_open(false)));
    document.on_entry_step(entry_step);
    document.on_browse_entry(browse_entry);
    document.on_preview_thumbnail(|_| refresh_previews());
    document.on_add_mirror(|| {
        with_edit(|edit| edit.mirrors.push(SharedString::new()));
    });
    document.on_remove_mirror(|index| {
        with_edit(|edit| remove_row(&edit.mirrors, index));
    });
    document.on_add_entry_header(|| {
        with_edit(|edit| edit.entry_headers.push(HeaderItem::default()));
    });
    document.on_remove_entry_header(|index| {
        with_edit(|edit| remove_row(&edit.entry_headers, index));
    });
    document.on_add_skip(|| {
        with_edit(|edit| edit.skips.push(SkipItem::default()));
    });
    document.on_remove_skip(|index| {
        with_edit(|edit| remove_row(&edit.skips, index));
    });
    document.on_copy_skips(copy_skips);
    document.on_use_position(|which| {
        with_app(|app| {
            let position = playlist_file::time_text(f64::from(app.global::<Player>().get_position()));
            if which == 0 {
                doc(app).set_e_start(position.into());
            } else {
                doc(app).set_e_end(position.into());
            }
        });
    });
    document.on_set_default_subtitle(|index| {
        with_edit(|edit| {
            for row in 0..edit.subtitles.row_count() {
                if let Some(mut item) = edit.subtitles.row_data(row) {
                    item.default = row == index as usize && !item.default;
                    edit.subtitles.set_row_data(row, item);
                }
            }
        });
    });
    document.on_remove_subtitle(|index| {
        with_edit(|edit| remove_row(&edit.subtitles, index));
    });
}

fn remove_row<T: Clone + 'static>(model: &VecModel<T>, index: i32) {
    if let Ok(index) = usize::try_from(index)
        && index < model.row_count()
    {
        model.remove(index);
    }
}

fn text(value: &Option<String>) -> SharedString {
    value.as_deref().unwrap_or("").into()
}

fn number<T: ToString>(value: Option<T>) -> SharedString {
    value.map(|value| value.to_string()).unwrap_or_default().into()
}

fn optional(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

// Paths typed in the dialogs are made absolute, like the ones read from a file.
fn optional_source(text: &str) -> Option<String> {
    let text = optional(text)?;
    if text.contains("://") {
        return Some(text);
    }
    Some(std::path::absolute(&text).map_or(text, |path| path.to_string_lossy().into_owned()))
}

fn parse_number<T: std::str::FromStr>(text: &str) -> Option<T> {
    text.trim().parse().ok()
}

/// Loads the images the dialogs show, once the loader has them.
pub fn refresh_previews() {
    with_app(|app| {
        let document = doc(app);
        if document.get_details_open() {
            match optional_source(&document.get_d_cover()).and_then(|cover| images::ready(&cover, Size::Poster)) {
                Some(image) => {
                    document.set_d_has_cover(true);
                    document.set_d_cover_image(slint::Image::from_rgba8(image));
                }
                None => document.set_d_has_cover(false),
            }
        }
        if document.get_entry_open() {
            match optional_source(&document.get_e_thumbnail()).and_then(|thumbnail| images::ready(&thumbnail, Size::Wide)) {
                Some(image) => {
                    document.set_e_has_thumbnail(true);
                    document.set_e_thumbnail_image(slint::Image::from_rgba8(image));
                }
                None => document.set_e_has_thumbnail(false),
            }
        }
    });
}

/// Asks whether to save an edited playlist before `then` goes ahead.
pub fn ask_to_save(then: Then) {
    let name = with_playlist(|playlist| match playlist.document().map(|document| &document.origin) {
        Some(Origin::File(path)) => path.file_name().map(|name| name.to_string_lossy().into_owned()),
        _ => None,
    })
    .flatten();
    with_edit(|edit| edit.then = Some(then));
    with_app(|app| {
        let document = doc(app);
        let title = match &name {
            Some(name) => format!("Save changes to {name}?"),
            None => "Save this playlist?".into(),
        };
        document.set_prompt_title(title.into());
        document.set_prompt_text("Your changes to the playlist aren't in the file yet.".into());
        document.set_prompt_open(true);
    });
}

fn prompt_answer(answer: i32) {
    let then = with_edit(|edit| edit.then.take()).flatten();
    with_app(|app| doc(app).set_prompt_open(false));
    let Some(then) = then else { return };
    match answer {
        1 => go_on(then),
        2 => save_or_ask(Some(then)),
        _ => {
            if matches!(then, Then::Open) {
                opener::release_held(false);
            }
        }
    }
}

fn go_on(then: Then) {
    playlist::forget_changes();
    match then {
        Then::Open => opener::release_held(true),
        Then::Close => with_app(|app| app.global::<WindowControls>().invoke_close()),
    }
}

// Saves in place when the playlist has its own file, otherwise through Save as. `then` runs once it's saved.
fn save_or_ask(then: Option<Then>) {
    if playlist::is_empty() {
        return;
    }
    if playlist::document_path().is_none() {
        save_as(then);
        return;
    }
    match playlist::save(None) {
        Ok(path) => {
            osd(format!("Saved {}", file_name(&path)));
            if let Some(then) = then {
                go_on(then);
            }
        }
        Err(err) => {
            warn!("{err}");
            osd(err);
            if matches!(then, Some(Then::Open)) {
                opener::release_held(false);
            }
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

fn safe_file_name(name: &str) -> String {
    let name: String = name.chars().map(|character| if "<>:\"/\\|?*".contains(character) || character.is_control() { '_' } else { character }).collect();
    let name = name.trim().trim_end_matches('.').to_string();
    if name.is_empty() { "Playlist".into() } else { name }
}

fn save_as(then: Option<Then>) {
    if playlist::is_empty() {
        return;
    }
    let name = format!("{}.{}", safe_file_name(&playlist::suggested_name()), playlist_file::EXTENSION);
    let folder = playlist::document_path().and_then(|path| path.parent().map(Path::to_path_buf));
    // The native dialog blocks its thread, which must not be the UI thread that draws the video.
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save playlist")
            .set_file_name(&name)
            .add_filter("Rona playlist", &[playlist_file::EXTENSION]);
        if let Some(folder) = folder {
            dialog = dialog.set_directory(folder);
        }
        let picked = dialog.save_file().map(|path| {
            if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case(playlist_file::EXTENSION)) {
                path
            } else {
                let mut with_extension = path.into_os_string();
                with_extension.push(".");
                with_extension.push(playlist_file::EXTENSION);
                PathBuf::from(with_extension)
            }
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(path) = picked else {
                if matches!(then, Some(Then::Open)) {
                    opener::release_held(false);
                }
                return;
            };
            match playlist::save(Some(path)) {
                Ok(path) => {
                    osd(format!("Saved {}", file_name(&path)));
                    if let Some(then) = then {
                        go_on(then);
                    }
                }
                Err(err) => {
                    warn!("{err}");
                    osd(err);
                    if matches!(then, Some(Then::Open)) {
                        opener::release_held(false);
                    }
                }
            }
        });
    });
}

fn toggle_autosave() {
    with_playlist(|playlist| {
        let mut file = playlist.document_mut().file.clone();
        file.playback.autosave = !file.playback.autosave;
        info!("autosave {}", if file.playback.autosave { "on" } else { "off" });
        playlist.set_file(file);
    });
    if playlist::document_path().is_none() && with_playlist(|playlist| playlist.document().is_some_and(|document| document.file.playback.autosave)).unwrap_or(false) {
        osd("Saves on its own once it has a file");
    }
}

fn export_m3u() {
    if playlist::is_empty() {
        return;
    }
    let text = playlist::m3u();
    let name = format!("{}.m3u8", safe_file_name(&playlist::suggested_name()));
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export as M3U")
            .set_file_name(&name)
            .add_filter("M3U playlist", &["m3u8", "m3u"])
            .save_file()
        else {
            return;
        };
        let result = std::fs::write(&path, text);
        let _ = slint::invoke_from_event_loop(move || match result {
            Ok(()) => osd(format!("Exported {}", file_name(&path))),
            Err(err) => osd(format!("Couldn't export: {err}")),
        });
    });
}

fn show_in_folder() {
    let Some(path) = playlist::document_path() else { return };
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(&path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(Path::new("."))).spawn()
    };
    if let Err(err) = result {
        warn!("could not show {} in its folder: {err}", path.display());
    }
}

fn find_file(index: usize) {
    let current = with_playlist(|playlist| playlist.meta_at(index).map(|meta| meta.entry.source.clone())).flatten();
    let Some(current) = current else { return };
    let folder = Path::new(&current).parent().map(Path::to_path_buf);
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().set_title("Find file");
        if let Some(name) = Path::new(&current).file_name() {
            dialog = dialog.set_file_name(name.to_string_lossy());
        }
        if let Some(folder) = folder.filter(|folder| folder.is_dir()) {
            dialog = dialog.set_directory(folder);
        }
        let Some(path) = dialog.pick_file() else { return };
        let _ = slint::invoke_from_event_loop(move || {
            let found = with_playlist(|playlist| {
                let old_folder = playlist.relink(index, &path)?;
                let new_folder = path.parent()?.to_path_buf();
                let others = playlist.missing_in(&old_folder, &new_folder);
                for (other, candidate) in &others {
                    playlist.relink(*other, candidate);
                }
                Some(others.len())
            })
            .flatten();
            match found {
                Some(0) | None => {}
                Some(1) => osd("Found 1 more in the same folder"),
                Some(count) => osd(format!("Found {count} more in the same folder")),
            }
        });
    });
}

fn kind_index(kind: Kind) -> i32 {
    match kind {
        Kind::Series => 0,
        Kind::Music => 1,
        Kind::Other => 2,
    }
}

fn open_details() {
    let file = with_playlist(|playlist| playlist.document_mut().file.clone());
    let names = with_playlist(|playlist| playlist.section_names()).unwrap_or_default();
    let count = with_playlist(|playlist| playlist.len()).unwrap_or(0);
    let Some(file) = file else { return };
    let origin = with_playlist(|playlist| playlist.document().map(|document| document.origin.clone())).flatten();
    with_edit(|edit| {
        let mut headers = file.headers.clone();
        let user_agent = headers.remove("User-Agent").unwrap_or_default();
        let referer = headers.remove("Referer").unwrap_or_default();
        edit.headers.set_vec(headers.into_iter().map(|(name, value)| HeaderItem { name: name.into(), value: value.into() }).collect::<Vec<_>>());
        edit.section_origins = (0..file.sections.len()).map(Some).collect();
        edit.sections.set_vec(
            file.sections
                .iter()
                .enumerate()
                .map(|(index, section)| SectionItem {
                    title: section.title.clone().unwrap_or_else(|| names.get(index).cloned().unwrap_or_default()).into(),
                    number: number(if file.kind == Kind::Music { section.disc } else { section.season }),
                    include: text(&section.include),
                })
                .collect::<Vec<_>>(),
        );
        let Some(app) = edit.app.upgrade() else { return };
        let document = doc(&app);
        document.set_type(kind_index(file.kind));
        document.set_d_title(text(&file.title));
        let series = file.series.clone().unwrap_or_default();
        document.set_d_series(text(&series.name));
        document.set_d_artist(text(&file.artist));
        document.set_d_album(text(&file.album));
        document.set_d_year(number(series.year.or(file.year)));
        document.set_d_description(text(&file.description));
        document.set_d_link(text(&file.link));
        document.set_d_author(text(&file.author));
        document.set_d_cover(text(&file.cover));
        document.set_d_background(text(&file.background));
        document.set_d_shuffle(match file.playback.shuffle {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
        document.set_d_repeat(match file.playback.repeat {
            None => 0,
            Some(RepeatMode::Off) => 1,
            Some(RepeatMode::Once) => 2,
            Some(RepeatMode::Forever) => 3,
        });
        document.set_d_start(number(file.playback.start.map(|start| start + 1)));
        document.set_d_autosave(file.playback.autosave);
        document.set_d_user_agent(user_agent.into());
        document.set_d_referer(referer.into());
        let place = match origin {
            Some(Origin::File(path)) => file_name(&path),
            Some(Origin::Link(url)) => crate::media::link_host(&url),
            _ => "Not saved yet".into(),
        };
        let entries = if count == 1 { "1 entry".to_string() } else { format!("{count} entries") };
        document.set_d_footer(format!("{place} · {entries}").into());
        document.set_details_page(0);
        document.set_details_open(true);
    });
    refresh_previews();
}

fn details_save() {
    let Some(file) = with_playlist(|playlist| playlist.document_mut().file.clone()) else { return };
    let mut file: PlaylistFile = file;
    let mut origins = Vec::new();
    with_edit(|edit| {
        let Some(app) = edit.app.upgrade() else { return };
        let document = doc(&app);
        file.kind = match document.get_type() {
            0 => Kind::Series,
            1 => Kind::Music,
            _ => Kind::Other,
        };
        file.title = optional(&document.get_d_title());
        file.description = optional(&document.get_d_description());
        file.link = optional(&document.get_d_link());
        file.author = optional(&document.get_d_author());
        file.cover = optional_source(&document.get_d_cover());
        file.background = optional_source(&document.get_d_background());
        let year = parse_number(&document.get_d_year());
        match file.kind {
            Kind::Series => {
                let mut series = file.series.take().unwrap_or_default();
                series.name = optional(&document.get_d_series());
                series.year = year;
                file.series = (series != Series::default()).then_some(series);
                file.year = None;
            }
            Kind::Music => {
                file.artist = optional(&document.get_d_artist());
                file.album = optional(&document.get_d_album());
                file.year = year;
            }
            Kind::Other => file.year = year,
        }
        file.playback.shuffle = match document.get_d_shuffle() {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        file.playback.repeat = match document.get_d_repeat() {
            1 => Some(RepeatMode::Off),
            2 => Some(RepeatMode::Once),
            3 => Some(RepeatMode::Forever),
            _ => None,
        };
        file.playback.start = parse_number::<usize>(&document.get_d_start()).filter(|start| *start > 0).map(|start| start - 1);
        file.playback.autosave = document.get_d_autosave();
        let mut headers: std::collections::BTreeMap<String, String> = edit
            .headers
            .iter()
            .filter(|header| !header.name.trim().is_empty())
            .map(|header| (header.name.trim().to_string(), header.value.to_string()))
            .collect();
        if let Some(user_agent) = optional(&document.get_d_user_agent()) {
            headers.insert("User-Agent".into(), user_agent);
        }
        if let Some(referer) = optional(&document.get_d_referer()) {
            headers.insert("Referer".into(), referer);
        }
        file.headers = headers;
        let old_sections = std::mem::take(&mut file.sections);
        for (row, origin) in edit.sections.iter().zip(edit.section_origins.iter()) {
            let mut section = origin.and_then(|origin| old_sections.get(origin).cloned()).unwrap_or_default();
            section.title = optional(&row.title);
            let value = parse_number(&row.number);
            if file.kind == Kind::Music {
                section.disc = value;
            } else {
                section.season = value;
            }
            file.sections.push(section);
        }
        origins = edit.section_origins.clone();
        document.set_details_open(false);
    });
    with_playlist(|playlist| {
        playlist.remap_sections(&origins);
        playlist.set_file(file);
    });
}

fn browse_image(which: i32) {
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .set_title(if which == 0 { "Choose a cover" } else { "Choose a background" })
            .add_filter("Images", &["jpg", "jpeg", "png", "webp"])
            .pick_file()
        else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            with_app(|app| {
                let text: SharedString = path.to_string_lossy().as_ref().into();
                if which == 0 {
                    doc(app).set_d_cover(text);
                } else {
                    doc(app).set_d_background(text);
                }
            });
            refresh_previews();
        });
    });
}

fn open_entry(index: usize) {
    let found = with_playlist(|playlist| {
        let meta = playlist.meta_at(index)?.clone();
        Some((meta, playlist.len(), playlist.section_names(), playlist.document().map_or(Kind::Other, |document| document.file.kind)))
    })
    .flatten();
    let Some((meta, count, sections, kind)) = found else { return };
    let entry = meta.entry.clone();
    with_edit(|edit| {
        edit.entry_index = index;
        edit.entry_base = Some(entry.clone());
        edit.mirrors.set_vec(entry.mirrors.iter().map(|mirror| SharedString::from(mirror.as_str())).collect::<Vec<_>>());
        edit.entry_headers.set_vec(
            entry.headers.iter().map(|(name, value)| HeaderItem { name: name.into(), value: value.into() }).collect::<Vec<_>>(),
        );
        edit.skips.set_vec(
            entry
                .skip
                .iter()
                .map(|skip| SkipItem {
                    name: text(&skip.name),
                    from: playlist_file::time_text(skip.from).into(),
                    to: playlist_file::time_text(skip.to).into(),
                })
                .collect::<Vec<_>>(),
        );
        edit.subtitles.set_vec(
            entry
                .subtitles
                .iter()
                .map(|subtitle| SubtitleItem {
                    source: subtitle.source.as_str().into(),
                    language: text(&subtitle.language),
                    default: subtitle.default,
                })
                .collect::<Vec<_>>(),
        );
        let Some(app) = edit.app.upgrade() else { return };
        let document = doc(&app);
        document.set_type(kind_index(kind));
        document.set_e_index(index as i32);
        document.set_e_count(count as i32);
        document.set_e_included(meta.included);
        document.set_e_message(if meta.included { "This entry comes from an included playlist, so changes here aren't saved." } else { "" }.into());
        document.set_e_title(text(&entry.title));
        document.set_e_season(number(entry.season));
        document.set_e_episode(number(entry.episode));
        document.set_e_absolute(number(entry.absolute));
        document.set_e_aired(text(&entry.aired));
        document.set_e_artist(text(&entry.artist));
        document.set_e_album(text(&entry.album));
        document.set_e_track(number(entry.track));
        document.set_e_disc(number(entry.disc));
        document.set_e_year(number(entry.year));
        document.set_e_description(text(&entry.description));
        document.set_e_tags(entry.tags.join(", ").into());
        document.set_e_thumbnail(text(&entry.thumbnail));
        let mut names = vec![SharedString::from("None")];
        names.extend(sections.iter().map(|name| SharedString::from(name.as_str())));
        document.set_section_names(ModelRc::new(VecModel::from(names)));
        document.set_e_section(meta.section.map_or(0, |section| section as i32 + 1));
        document.set_e_source(entry.source.as_str().into());
        document.set_e_playlist_headers(entry.use_playlist_headers != Some(false));
        document.set_e_start(entry.start.map(playlist_file::time_text).unwrap_or_default().into());
        document.set_e_end(entry.end.map(playlist_file::time_text).unwrap_or_default().into());
        document.set_e_volume(number(entry.volume));
        document.set_e_audio_language(text(&entry.tracks.audio));
        document.set_e_subtitle_language(text(&entry.tracks.subtitles));
        document.set_e_extra_audio(text(&entry.audio));
        document.set_entry_open(true);
    });
    refresh_previews();
}

// Reads the entry dialog back into the entry it was opened for.
fn entry_save() {
    let read = with_edit(|edit| {
        let app = edit.app.upgrade()?;
        let document = doc(&app);
        if document.get_e_included() {
            return None;
        }
        let mut entry = edit.entry_base.clone()?;
        let source = document.get_e_source().trim().to_string();
        entry.title = optional(&document.get_e_title());
        entry.season = parse_number(&document.get_e_season());
        entry.episode = parse_number(&document.get_e_episode());
        entry.absolute = parse_number(&document.get_e_absolute());
        entry.aired = optional(&document.get_e_aired());
        entry.artist = optional(&document.get_e_artist());
        entry.album = optional(&document.get_e_album());
        entry.track = parse_number(&document.get_e_track());
        entry.disc = parse_number(&document.get_e_disc());
        entry.year = parse_number(&document.get_e_year());
        entry.description = optional(&document.get_e_description());
        entry.tags = document.get_e_tags().split(',').filter_map(optional).collect();
        entry.thumbnail = optional_source(&document.get_e_thumbnail());
        entry.mirrors = edit.mirrors.iter().filter_map(|mirror| optional_source(&mirror)).collect();
        entry.use_playlist_headers = (!document.get_e_playlist_headers()).then_some(false);
        entry.headers = edit
            .entry_headers
            .iter()
            .filter(|header| !header.name.trim().is_empty())
            .map(|header| (header.name.trim().to_string(), header.value.to_string()))
            .collect();
        entry.start = playlist_file::parse_time(&document.get_e_start());
        entry.end = playlist_file::parse_time(&document.get_e_end());
        entry.skip = edit
            .skips
            .iter()
            .filter_map(|skip| {
                let (from, to) = (playlist_file::parse_time(&skip.from)?, playlist_file::parse_time(&skip.to)?);
                (to > from).then(|| Skip { name: optional(&skip.name), from, to, ..Skip::default() })
            })
            .collect();
        entry.volume = parse_number::<f64>(&document.get_e_volume()).filter(|volume| *volume != 0.0);
        let old_subtitles = std::mem::take(&mut entry.subtitles);
        entry.subtitles = edit
            .subtitles
            .iter()
            .map(|row| {
                let mut subtitle = old_subtitles
                    .iter()
                    .find(|old| old.source == row.source.as_str())
                    .cloned()
                    .unwrap_or_else(|| Subtitle { source: row.source.to_string(), ..Subtitle::default() });
                subtitle.language = optional(&row.language);
                subtitle.default = row.default;
                subtitle
            })
            .collect();
        entry.tracks.audio = optional(&document.get_e_audio_language());
        entry.tracks.subtitles = optional(&document.get_e_subtitle_language());
        entry.audio = optional_source(&document.get_e_extra_audio());
        let section = usize::try_from(document.get_e_section() - 1).ok();
        let new_source = optional_source(&source).filter(|new| *new != entry.source);
        Some((edit.entry_index, entry, section, new_source))
    })
    .flatten();
    let Some((index, entry, section, new_source)) = read else { return };
    with_playlist(|playlist| {
        playlist.set_meta(index, entry, section);
        if let Some(source) = new_source {
            playlist.change_source(index, source);
        }
    });
}

fn entry_step(step: i32) {
    entry_save();
    let Some((index, count)) = with_edit(|edit| edit.entry_index).zip(with_playlist(|playlist| playlist.len())) else { return };
    let next = index as i32 + step;
    if next >= 0 && (next as usize) < count {
        open_entry(next as usize);
    }
}

fn browse_entry(which: i32) {
    std::thread::spawn(move || {
        let dialog = rfd::FileDialog::new();
        let dialog = match which {
            0 => dialog.set_title("Choose a thumbnail").add_filter("Images", &["jpg", "jpeg", "png", "webp"]),
            1 => dialog.set_title("Choose the file").add_filter("Video and audio", crate::media::dialog_extensions().as_slice()),
            2 => dialog.set_title("Choose an audio file").add_filter("Audio", &crate::extensions::AUDIO_EXTENSIONS),
            _ => dialog.set_title("Add subtitles").add_filter("Subtitles", &["srt", "ass", "ssa", "vtt", "sub", "sup", "idx"]),
        };
        let Some(path) = dialog.add_filter("All files", &["*"]).pick_file() else { return };
        let _ = slint::invoke_from_event_loop(move || {
            let text: SharedString = path.to_string_lossy().as_ref().into();
            match which {
                3 => {
                    with_edit(|edit| edit.subtitles.push(SubtitleItem { source: text, ..SubtitleItem::default() }));
                }
                _ => with_app(|app| {
                    let document = doc(app);
                    match which {
                        0 => document.set_e_thumbnail(text),
                        1 => document.set_e_source(text),
                        _ => document.set_e_extra_audio(text),
                    }
                }),
            }
            refresh_previews();
        });
    });
}

fn copy_skips() {
    entry_save();
    let Some(index) = with_edit(|edit| edit.entry_index) else { return };
    let count = with_playlist(|playlist| {
        let others = playlist.same_section(index).len();
        playlist.copy_skips(index);
        others
    })
    .unwrap_or(0);
    osd(if count == 1 { "Copied to 1 more entry".into() } else { format!("Copied to {count} more entries") });
}
