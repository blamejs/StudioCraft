//! Live captions on the program: the text the speech recognizer heard, drawn like a text source.
//!
//! The studio's captions runtime writes what it hears to a [`CaptionBoard`]: each finished phrase
//! as a line, and the phrase being spoken as a partial line that the next update replaces. Every
//! Live Captions source draws the board's last lines, wrapped at a number of characters, and
//! redraws only when the text it shows changes. Lines older than the source's "Clear after"
//! setting are not shown. A source shows nothing while captions are stopped and the board is
//! empty.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ab_glyph::FontRef;
use studiocraft_media::Tick;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::text::{Style, load_font, render};
use crate::uint;

/// The source's plug-in id.
pub const CAPTIONS_ID: &str = "captions";
/// How often a source looks at the board.
const CHECK: Duration = Duration::from_millis(100);
/// Finished lines a board keeps.
const KEPT_LINES: usize = 16;
/// The longest line a board keeps, in characters.
const MAX_LINE_CHARS: usize = 2_000;
/// The languages offered, as ISO 639-1 codes with their English names; `auto` detects it.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Detect"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("tr", "Turkish"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("zh", "Chinese"),
];

/// What a board holds at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardSnapshot {
    /// Finished lines, oldest first, each with the studio clock's time it was written.
    pub lines: Vec<(String, Tick)>,
    /// The phrase being spoken, with the time of its last update.
    pub partial: Option<(String, Tick)>,
    /// Counts every change.
    pub generation: u64,
}

/// The text live captions heard, shared between the captions runtime and the sources.
#[derive(Default)]
pub struct CaptionBoard {
    inner: Mutex<BoardSnapshot>,
}

fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

impl CaptionBoard {
    pub fn new() -> CaptionBoard {
        CaptionBoard::default()
    }

    /// The process's one board, which the studio writes and the registered sources read.
    pub fn system() -> Arc<CaptionBoard> {
        static BOARD: OnceLock<Arc<CaptionBoard>> = OnceLock::new();
        Arc::clone(BOARD.get_or_init(|| Arc::new(CaptionBoard::new())))
    }

    fn change(&self, f: impl FnOnce(&mut BoardSnapshot)) {
        let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut b);
        b.generation = b.generation.wrapping_add(1);
    }

    /// Show `text` as the phrase being spoken; empty text removes the partial line.
    pub fn set_partial(&self, text: &str, at: Tick) {
        let text = clip(text);
        self.change(|b| b.partial = (!text.is_empty()).then_some((text, at)));
    }

    /// Add a finished line and remove the partial line it replaces.
    pub fn push_final(&self, text: &str, at: Tick) {
        let text = clip(text);
        self.change(|b| {
            b.partial = None;
            if !text.is_empty() {
                b.lines.push((text, at));
                let extra = b.lines.len().saturating_sub(KEPT_LINES);
                b.lines.drain(..extra);
            }
        });
    }

    /// Remove every line.
    pub fn clear(&self) {
        self.change(|b| {
            b.lines.clear();
            b.partial = None;
        });
    }

    pub fn snapshot(&self) -> BoardSnapshot {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The text a source shows: the lines (and the partial line) written within `clear_after` of
/// `now` (all of them when `clear_after` is zero), wrapped at `max_chars` characters, and the last
/// `lines` lines of that.
pub fn caption_text(board: &BoardSnapshot, now: Tick, lines: usize, max_chars: usize, clear_after: Duration) -> String {
    let fresh = |at: Tick| {
        clear_after.is_zero() || {
            let limit = Tick::from_units(i64::try_from(clear_after.as_millis()).unwrap_or(i64::MAX), 1000);
            now.saturating_sub(at).0 <= limit.0
        }
    };
    let mut wrapped: VecDeque<String> = VecDeque::new();
    let texts = board.lines.iter().chain(board.partial.iter()).filter(|(_, at)| fresh(*at)).map(|(t, _)| t.as_str());
    for t in texts {
        for line in wrap(t, max_chars.max(1)) {
            wrapped.push_back(line);
            if wrapped.len() > lines.max(1) {
                wrapped.pop_front();
            }
        }
    }
    Vec::from(wrapped).join("\n")
}

/// `text` in lines of at most `max` characters, broken at spaces; a word longer than a line is
/// broken inside it (text written without spaces, such as Japanese, wraps that way).
pub fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    let mut len = 0usize;
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        if len > 0 && len + 1 + word.len() <= max {
            line.push(' ');
            line.extend(&word);
            len += 1 + word.len();
            continue;
        }
        if len > 0 {
            out.push(std::mem::take(&mut line));
        }
        while word.len() > max {
            let rest = word.split_off(max);
            out.push(word.into_iter().collect());
            word = rest;
        }
        line.extend(&word);
        len = word.len();
    }
    if len > 0 {
        out.push(line);
    }
    out
}

/// The Live Captions source.
pub struct CaptionsSource {
    board: Arc<CaptionBoard>,
}

impl CaptionsSource {
    pub fn new(board: Arc<CaptionBoard>) -> CaptionsSource {
        CaptionsSource { board }
    }
}

impl Plugin for CaptionsSource {
    fn id(&self) -> &'static str {
        CAPTIONS_ID
    }
    fn name(&self) -> &'static str {
        "Live Captions"
    }
    fn schema(&self) -> SettingsSchema {
        let models: Vec<ChoiceOption> = craft_speech::models::catalog().iter().map(|m| ChoiceOption::new(m.id, m.name)).collect();
        let languages: Vec<ChoiceOption> = LANGUAGES.iter().map(|(code, name)| ChoiceOption::new(*code, *name)).collect();
        let align = vec![ChoiceOption::new("left", "Left"), ChoiceOption::new("center", "Center"), ChoiceOption::new("right", "Right")];
        SettingsSchema::new(vec![
            Field::new("model", "Speech model", FieldKind::Choice { options: models }, craft_speech::models::DEFAULT_MODEL),
            Field::new("language", "Language", FieldKind::Choice { options: languages }, "auto"),
            Field::new("lines", "Lines", FieldKind::Int { min: 1, max: 4 }, 2),
            Field::new("max_chars", "Characters per line", FieldKind::Int { min: 16, max: 80 }, 42),
            Field::new("clear_after", "Clear after (seconds, 0 keeps the text)", FieldKind::Int { min: 0, max: 60 }, 4),
            Field::new("font", "Font", FieldKind::Font, ""),
            Field::new("size", "Size", FieldKind::Int { min: 6, max: 1024 }, 48),
            Field::new("color", "Color", FieldKind::Color, "#ffffffff"),
            Field::new("align", "Alignment", FieldKind::Choice { options: align }, "center"),
            Field::new("outline", "Outline width", FieldKind::Int { min: 0, max: 64 }, 2),
            Field::new("outline_color", "Outline color", FieldKind::Color, "#000000ff"),
            Field::new("background", "Background", FieldKind::Color, "#000000b3"),
        ])
    }
}

impl Source for CaptionsSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance { board: Arc::clone(&self.board), settings: settings.clone(), shown: None, font: None }))
    }
}

struct Instance {
    board: Arc<CaptionBoard>,
    settings: Settings,
    /// The text last drawn; `None` after a settings change, so the next check draws.
    shown: Option<String>,
    /// The font last loaded, with the family it was loaded for.
    font: Option<(String, craft_font_catalog::FontData)>,
}

impl Instance {
    fn draw(&mut self, text: &str, io: &SourceIo) -> Result<(), PluginError> {
        let family = self.settings.str("font").unwrap_or_default().to_string();
        if self.font.as_ref().is_none_or(|(f, _)| *f != family) {
            self.font = Some((family.clone(), load_font(&family)?));
        }
        let Some((_, font)) = &self.font else { return Ok(()) };
        let face = FontRef::try_from_slice_and_index(&font.bytes, font.index)
            .map_err(|e| PluginError::Failed(format!("the font could not be read: {e}")))?;
        let style = Style::read(&self.settings);
        let mut frame = io.video.frame();
        let (w, h) = render(&face, text, &style, &mut frame.data)?;
        frame.width = w;
        frame.height = h;
        frame.format = PixelFormat::Rgba8;
        frame.pts = io.clock.now();
        io.video.publish(VideoContent::Frame(frame));
        Ok(())
    }
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        Pace::Every(CHECK)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        let s = &self.settings;
        let lines = uint(s, "lines", 2, 1, 4) as usize;
        let max_chars = uint(s, "max_chars", 42, 16, 80) as usize;
        let clear_after = Duration::from_secs(u64::from(uint(s, "clear_after", 4, 0, 60)));
        let text = caption_text(&self.board.snapshot(), io.clock.now(), lines, max_chars, clear_after);
        if self.shown.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        if text.is_empty() {
            io.video.publish(VideoContent::Empty);
            self.shown = Some(text);
            return Ok(());
        }
        // A failed draw is tried again on the next call.
        self.shown = None;
        if let Err(e) = self.draw(&text, io) {
            io.video.publish(VideoContent::Empty);
            return Err(e);
        }
        self.shown = Some(text);
        Ok(())
    }
    fn update(&mut self, settings: &Settings, _io: &SourceIo) -> Result<(), PluginError> {
        self.settings = settings.clone();
        self.shown = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studiocraft_media::clock::MasterClock;

    fn at(ms: i64) -> Tick {
        Tick::from_units(ms, 1000)
    }

    #[test]
    fn lines_wrap_at_spaces_and_long_words_break() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("  spaced   out  ", 80), ["spaced out"]);
        assert_eq!(wrap("abcdefghij k", 4), ["abcd", "efgh", "ij k"]);
        assert_eq!(wrap("今日はいい天気ですね", 4), ["今日はい", "い天気で", "すね"]);
        assert!(wrap("", 10).is_empty());
        assert_eq!(wrap("x", 0), ["x"]);
    }

    #[test]
    fn the_last_lines_are_shown_and_old_ones_clear() {
        let board = CaptionBoard::new();
        board.push_final("one two three", at(0));
        board.push_final("four five six", at(3000));
        board.set_partial("seven eight", at(5000));
        let s = board.snapshot();
        assert_eq!(caption_text(&s, at(5000), 2, 80, Duration::from_secs(4)), "four five six\nseven eight");
        assert_eq!(caption_text(&s, at(5000), 1, 80, Duration::ZERO), "seven eight");
        assert_eq!(caption_text(&s, at(5000), 4, 9, Duration::ZERO), "four five\nsix\nseven\neight");
        assert_eq!(caption_text(&s, at(8500), 2, 80, Duration::from_secs(4)), "seven eight");
        assert_eq!(caption_text(&s, at(60_000), 2, 80, Duration::from_secs(4)), "");
        board.push_final("seven eight nine", at(5500));
        assert_eq!(board.snapshot().partial, None, "a final line replaces the partial one");
    }

    #[test]
    fn a_board_keeps_a_bounded_amount() {
        let board = CaptionBoard::new();
        for i in 0..100 {
            board.push_final(&format!("line {i}"), at(i));
        }
        board.push_final(&"w".repeat(10_000), at(200));
        board.set_partial("   ", at(300));
        let s = board.snapshot();
        assert_eq!(s.lines.len(), KEPT_LINES);
        assert_eq!(s.lines.last().map(|l| l.0.chars().count()), Some(MAX_LINE_CHARS));
        assert_eq!(s.partial, None);
        board.clear();
        assert!(board.snapshot().lines.is_empty());
        assert!(board.snapshot().generation > s.generation);
    }

    #[test]
    fn a_source_redraws_only_when_its_text_changes() {
        let board = Arc::new(CaptionBoard::new());
        let io = SourceIo::new(MasterClock::new());
        let source = CaptionsSource::new(Arc::clone(&board));
        let settings = source.schema().defaults().with("clear_after", 0);
        let mut i = source.create(&settings, &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "nothing heard yet");
        i.produce(&io).unwrap();
        assert!(io.video.take().is_none(), "the same text is not drawn again");
        board.set_partial("hello", io.clock.now());
        if load_font("").is_err() {
            assert!(i.produce(&io).is_err());
            assert!(i.produce(&io).is_err(), "a failed draw is tried again");
            return;
        }
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        i.produce(&io).unwrap();
        assert!(io.video.take().is_none());
        i.update(&settings.clone().with("size", 60), &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))), "a settings change draws again");
        board.clear();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
    }

    #[test]
    fn the_schema_offers_every_catalog_model() {
        let schema = CaptionsSource::new(CaptionBoard::system()).schema();
        let defaults = schema.defaults();
        assert_eq!(defaults.str("model"), Some(craft_speech::models::DEFAULT_MODEL));
        assert_eq!(defaults.str("language"), Some("auto"));
        let json = serde_json::to_string(&schema).unwrap_or_default();
        for m in craft_speech::models::catalog() {
            assert!(json.contains(m.id), "{}", m.id);
        }
    }
}
