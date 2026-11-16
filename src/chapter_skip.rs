use std::collections::HashSet;

use slint::{ComponentHandle, Model};

use crate::mpv::Mpv;
use crate::{AppWindow, Player, Settings, log_error};

#[derive(Default)]
pub struct ChapterSkip {
    current: Option<usize>,
    // Each chapter is looked at once per file, so going back to an opening on purpose plays it.
    checked: HashSet<usize>,
}

impl ChapterSkip {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn set_current(&mut self, index: f64) {
        self.current = (index >= 0.0).then_some(index as usize);
    }

    // Also runs on position updates, because the chapter list is read in the background and can arrive late.
    pub fn check(&mut self, mpv: &Mpv, app: &AppWindow) {
        let settings = app.global::<Settings>();
        if !settings.get_skip_chapters() {
            return;
        }
        let Some(index) = self.current.filter(|index| !self.checked.contains(index)) else { return };
        let chapters = app.global::<Player>().get_chapters();
        if index >= chapters.row_count() {
            return;
        }
        let names = skip_names(&settings.get_skip_chapter_titles());
        let skipped = |index: usize| chapters.row_data(index).is_some_and(|chapter| title_matches(&chapter.title, &names));
        let resume_at = (index..chapters.row_count()).find(|&later| !skipped(later));
        self.checked.insert(index);
        self.checked.extend(index..resume_at.unwrap_or(chapters.row_count()));
        match resume_at {
            Some(later) if later == index => {}
            Some(later) => log_error(mpv.command(&["set", "chapter", &later.to_string()])),
            None => log_error(mpv.command(&["seek", "100", "absolute-percent"])),
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn skip_names(text: &str) -> Vec<Vec<String>> {
    text.split(',').map(words).filter(|name| !name.is_empty()).collect()
}

fn title_matches(title: &str, names: &[Vec<String>]) -> bool {
    let title = words(title);
    names.iter().any(|name| title.windows(name.len()).any(|run| run == name.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(title: &str, names: &str) -> bool {
        title_matches(title, &skip_names(names))
    }

    #[test]
    fn matches_names_as_whole_words_in_real_chapter_titles() {
        let names = "Opening, Ending, OP, ED, Preview";
        assert!(matches("Opening", names));
        assert!(matches("OP", names));
        assert!(matches("Opening Song", names));
        assert!(matches("ED - \"Hikari\"", names));
        assert!(matches("Next Episode Preview", names));
        assert!(!matches("Part A", names));
        assert!(!matches("Edward's Plan", names));
        assert!(!matches("Operation Start", names));
    }

    #[test]
    fn multi_word_names_need_the_words_in_order() {
        assert!(matches("Next Episode Preview", "episode preview"));
        assert!(!matches("Preview of the episode", "episode preview"));
    }

    #[test]
    fn empty_names_match_nothing() {
        assert!(!matches("Opening", ""));
        assert!(!matches("Opening", " , ,"));
        assert!(!matches("", "Opening"));
    }
}
