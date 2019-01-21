#![allow(dead_code)]

mod prelude;

use self::prelude::*;

use std::path::Path;

use std::cmp::min;
use std::io::{self, Write};
use structopt::StructOpt;
use termion::color;
use termion::event::Event;
use termion::input::TermRead;
use termion::raw::IntoRawMode;
use termion::screen::*;

use ropey::Rope;

mod coord;
mod idx;
mod mode;
mod opts;
mod selection;

use crate::{coord::*, idx::Idx, mode::*, selection::*};

/// Keep track of color codes in output
///
/// This is to save on unnecessary output to terminal
/// which can generated flickering etc.
#[derive(Default)]
struct CachingAnsciWriter {
    buf: Vec<u8>,
    cur_fg: Option<u8>,
    cur_bg: Option<u8>,
}

impl CachingAnsciWriter {
    fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    fn reset_color(&mut self) -> io::Result<()> {
        if self.cur_fg.is_some() {
            self.cur_fg = None;
            write!(&mut self.buf, "{}", color::Fg(color::Reset),)?;
        }

        if self.cur_bg.is_some() {
            self.cur_bg = None;
            write!(&mut self.buf, "{}", color::Bg(color::Reset),)?;
        }
        Ok(())
    }

    fn change_color(&mut self, fg: color::AnsiValue, bg: color::AnsiValue) -> io::Result<()> {
        if self.cur_fg != Some(fg.0) {
            self.cur_fg = Some(fg.0);
            write!(&mut self.buf, "{}", color::Fg(fg),)?;
        }

        if self.cur_bg != Some(bg.0) {
            self.cur_bg = Some(bg.0);
            write!(&mut self.buf, "{}", color::Bg(bg),)?;
        }
        Ok(())
    }
}

impl io::Write for CachingAnsciWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf.flush()
    }
}

/// Buffer
///
/// A file opened for edition + some state around
#[derive(Debug, Clone)]
struct Buffer {
    text: ropey::Rope,
    selections: Vec<SelectionUnaligned>,
    primary_sel_i: usize,
}

impl Default for Buffer {
    fn default() -> Self {
        let sel = SelectionUnaligned::default();
        Self {
            text: Rope::default(),
            selections: vec![sel],
            primary_sel_i: 0,
        }
    }
}

impl Buffer {
    fn from_text(text: Rope) -> Self {
        Self { text, ..default() }
    }

    fn for_each_selection<F, R>(&self, mut f: F) -> Vec<R>
    where
        F: FnMut(&SelectionUnaligned, &Rope) -> R,
    {
        let Self {
            ref selections,
            ref text,
            ..
        } = *self;

        selections.iter().map(|sel| f(sel, text)).collect()
    }

    fn for_each_selection_mut<F, R>(&mut self, mut f: F) -> Vec<R>
    where
        F: FnMut(&mut SelectionUnaligned, &mut Rope) -> R,
    {
        let Self {
            ref mut selections,
            ref mut text,
            ..
        } = *self;

        selections.iter_mut().map(|sel| f(sel, text)).collect()
    }

    fn for_each_enumerated_selection<F, R>(&self, mut f: F) -> Vec<R>
    where
        F: FnMut(usize, &SelectionUnaligned, &Rope) -> R,
    {
        let Self {
            ref selections,
            ref text,
            ..
        } = *self;

        selections
            .iter()
            .enumerate()
            .map(|(i, sel)| f(i, sel, text))
            .collect()
    }
    fn for_each_enumerated_selection_mut<F, R>(&mut self, mut f: F) -> Vec<R>
    where
        F: FnMut(usize, &mut SelectionUnaligned, &mut Rope) -> R,
    {
        let Self {
            ref mut selections,
            ref mut text,
            ..
        } = *self;

        selections
            .iter_mut()
            .enumerate()
            .map(|(i, sel)| f(i, sel, text))
            .collect()
    }

    fn is_idx_selected(&self, idx: Idx) -> bool {
        self.selections
            .iter()
            .any(|sel| sel.align(&self.text).is_idx_inside(idx))
    }

    fn reverse_selections(&mut self) {
        self.for_each_selection_mut(|sel, _text| *sel = sel.reversed());
    }

    fn insert(&mut self, ch: char) {
        let mut insertion_points = self.for_each_enumerated_selection(|i, sel, text| {
            (i, sel.cursor.trim_column_to_buf(text).to_idx(text))
        });
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            self.text.insert_char(idx.0, ch);
            for fixing_i in 0..=i {
                let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                fixing_sel.cursor = fixing_sel.cursor.forward(1, &self.text);
                *fixing_sel = fixing_sel.collapsed();
            }
        }
    }

    fn delete(&mut self) -> Vec<Rope> {
        let res = self.for_each_enumerated_selection_mut(|i, sel, text| {
            let range = sel.align(text).sorted_range_usize();
            let yanked = text.slice(range.clone()).into();
            *sel = sel.collapsed();
            (yanked, i, range)
        });
        let mut removal_points = vec![];
        let mut yanked = vec![];

        for (y, i, r) in res.into_iter() {
            removal_points.push((i, r));
            yanked.push(y);
        }

        self.remove_ranges(removal_points);

        yanked
    }

    fn yank(&mut self) -> Vec<Rope> {
        self.for_each_selection_mut(|sel, text| {
            let range = sel.align(text).sorted_range_usize();
            text.slice(range).into()
        })
    }

    fn paste(&mut self, yanked: &[Rope]) {
        let mut insertion_points = self.for_each_enumerated_selection(|i, sel, text| {
            (i, sel.cursor.trim_column_to_buf(text).to_idx(text))
        });
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            if let Some(to_yank) = yanked.get(i) {
                for chunk in to_yank.chunks() {
                    self.text.insert(idx.0, chunk);
                }
                {
                    let fixing_sel = &mut self.selections[insertion_points[i].0];
                    if fixing_sel.align(&self.text).is_forward().unwrap_or(true) {
                        fixing_sel.anchor = fixing_sel.cursor;
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward(to_yank.len_chars(), &self.text);
                    } else {
                        fixing_sel.anchor =
                            fixing_sel.cursor.forward(to_yank.len_chars(), &self.text);
                    }
                }
                for fixing_i in 0..i {
                    let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                    if *idx
                        <= fixing_sel
                            .cursor
                            .trim_column_to_buf(&self.text)
                            .to_idx(&self.text)
                    {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward(to_yank.len_chars(), &self.text);
                    }
                    if *idx
                        <= fixing_sel
                            .anchor
                            .trim_column_to_buf(&self.text)
                            .to_idx(&self.text)
                    {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward(to_yank.len_chars(), &self.text);
                    }
                }
            }
        }
    }

    fn paste_extend(&mut self, yanked: &[Rope]) {
        let mut insertion_points = self.for_each_enumerated_selection(|i, sel, text| {
            (i, sel.cursor.trim_column_to_buf(text).to_idx(text))
        });
        insertion_points.sort_by_key(|&(_, idx)| idx);
        insertion_points.reverse();

        // we insert from the back, fixing idx past the insertion every time
        // this is O(n^2) while it could be O(n)
        for (i, (_, idx)) in insertion_points.iter().enumerate() {
            if let Some(to_yank) = yanked.get(i) {
                for chunk in to_yank.chunks() {
                    self.text.insert(idx.0, chunk);
                }
                {
                    let fixing_sel = &mut self.selections[insertion_points[i].0];
                    if fixing_sel.align(&self.text).is_forward().unwrap_or(true) {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward(to_yank.len_chars(), &self.text);
                    } else {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward(to_yank.len_chars(), &self.text);
                    }
                }
                for fixing_i in 0..i {
                    let fixing_sel = &mut self.selections[insertion_points[fixing_i].0];
                    if *idx
                        <= fixing_sel
                            .cursor
                            .trim_column_to_buf(&self.text)
                            .to_idx(&self.text)
                    {
                        fixing_sel.cursor =
                            fixing_sel.cursor.forward(to_yank.len_chars(), &self.text);
                    }
                    if *idx
                        <= fixing_sel
                            .anchor
                            .trim_column_to_buf(&self.text)
                            .to_idx(&self.text)
                    {
                        fixing_sel.anchor =
                            fixing_sel.anchor.forward(to_yank.len_chars(), &self.text);
                    }
                }
            }
        }
    }

    /// Remove text at given ranges
    ///
    /// `removal_points` contains list of `(selection_index, range)`,
    fn remove_ranges(&mut self, mut removal_points: Vec<(usize, std::ops::Range<usize>)>) {
        removal_points.sort_by_key(|&(_, ref range)| range.start);
        removal_points.reverse();

        // we remove from the back, fixing idx past the removal every time
        // this is O(n^2) while it could be O(n)
        for (_, (_, range)) in removal_points.iter().enumerate() {
            self.sub_to_every_selection_after(Idx(range.start), range.len());
            // remove has to be after fixes, otherwise to_idx conversion
            // will use the new buffer content, which will give wrong results
            self.text.remove(range.clone());
        }
    }

    fn backspace(&mut self) {
        let removal_points = self.for_each_enumerated_selection_mut(|i, sel, text| {
            let sel_aligned = sel.align(text);
            let range = (sel_aligned.cursor.0 - 1)..sel_aligned.cursor.0;
            *sel = sel.collapsed();

            (i, range)
        });

        self.remove_ranges(removal_points);
    }

    fn add_to_every_selection_after(&mut self, idx: Idx, offset: usize) {
        self.for_each_selection_mut(|sel, text| {
            let cursor_idx = sel.cursor.to_idx(text);
            let anchor_idx = sel.cursor.to_idx(text);

            if idx <= cursor_idx {
                sel.cursor = Idx(cursor_idx.0.saturating_add(offset))
                    .to_coord(text)
                    .into();
            }
            if idx <= anchor_idx {
                sel.anchor = Idx(anchor_idx.0.saturating_add(offset))
                    .to_coord(text)
                    .into();
            }
        });
    }

    fn sub_to_every_selection_after(&mut self, idx: Idx, offset: usize) {
        self.for_each_selection_mut(|sel, text| {
            let cursor_idx = sel.cursor.to_idx(text);
            let anchor_idx = sel.anchor.to_idx(text);
            if idx < cursor_idx {
                sel.cursor = Idx(cursor_idx.0.saturating_sub(offset))
                    .to_coord(text)
                    .into();
            }
            if idx < anchor_idx {
                sel.anchor = Idx(anchor_idx.0.saturating_sub(offset))
                    .to_coord(text)
                    .into();
            }
        });
    }

    fn move_cursor<F>(&mut self, f: F)
    where
        F: Fn(Coord, &Rope) -> Coord,
    {
        self.for_each_selection_mut(|sel, text| {
            let new_cursor = f(sel.cursor, text);
            sel.anchor = sel.cursor;
            sel.cursor = new_cursor;
        });
    }

    fn move_cursor_2<F>(&mut self, f: F)
    where
        F: Fn(Coord, &Rope) -> (Coord, Coord),
    {
        self.for_each_selection_mut(|sel, text| {
            let (new_anchor, new_cursor) = f(sel.cursor, text);
            sel.anchor = new_anchor;
            sel.cursor = new_cursor;
        });
    }

    fn extend_cursor<F>(&mut self, f: F)
    where
        F: Fn(Coord, &Rope) -> Coord,
    {
        self.for_each_selection_mut(|sel, text| {
            sel.cursor = f(sel.cursor, text);
        });
    }

    fn extend_cursor_2<F>(&mut self, f: F)
    where
        F: Fn(Coord, &Rope) -> (Coord, Coord),
    {
        self.for_each_selection_mut(|sel, text| {
            let (_new_anchor, new_cursor) = f(sel.cursor, text);
            sel.cursor = new_cursor;
        });
    }
    fn change_selection<F>(&mut self, f: F)
    where
        F: Fn(Coord, Coord, &Rope) -> (Coord, Coord),
    {
        self.for_each_selection_mut(|sel, text| {
            let (new_cursor, new_anchor) = f(sel.cursor, sel.anchor, text);
            sel.anchor = new_anchor;
            sel.cursor = new_cursor;
        });
    }

    fn move_cursor_backward(&mut self, n: usize) {
        self.move_cursor(|coord, text| coord.backward(n, text));
    }

    fn move_cursor_forward(&mut self, n: usize) {
        self.move_cursor(|coord, text| coord.forward(n, text));
    }

    fn move_cursor_down(&mut self, n: usize) {
        self.move_cursor(|coord, text| coord.down_unaligned(n, text));
    }

    fn move_cursor_up(&mut self, n: usize) {
        self.move_cursor(|coord, text| coord.up_unaligned(n, text));
    }

    fn extend_cursor_backward(&mut self, n: usize) {
        self.extend_cursor(|coord, text| coord.backward(n, text));
    }

    fn extend_cursor_forward(&mut self, n: usize) {
        self.extend_cursor(|coord, text| coord.forward(n, text));
    }

    fn extend_cursor_down(&mut self, n: usize) {
        self.extend_cursor(|coord, text| coord.down_unaligned(n, text));
    }

    fn extend_cursor_up(&mut self, n: usize) {
        self.extend_cursor(|coord, text| coord.up_unaligned(n, text));
    }
    fn move_cursor_forward_word(&mut self) {
        self.move_cursor_2(Coord::forward_word)
    }

    fn move_cursor_backward_word(&mut self) {
        self.move_cursor_2(Coord::backward_word)
    }

    fn cursor_pos(&self) -> Coord {
        self.selections[0].cursor.trim_column_to_buf(&self.text)
    }

    fn move_line(&mut self) {
        self.change_selection(|cursor, _anchor, text| {
            (
                cursor.forward_past_line_end(text),
                cursor.backward_to_line_start(text),
            )
        });
    }

    fn extend_line(&mut self) {
        self.change_selection(|cursor, anchor, text| {
            let anchor = min(cursor, anchor);

            (
                cursor.forward_past_line_end(text),
                if anchor.column == 0 {
                    anchor
                } else {
                    anchor.backward_to_line_start(text)
                },
            )
        });
    }

    fn select_all(&mut self) {
        self.selections = vec![SelectionUnaligned::from_selection(
            if self.selections[self.primary_sel_i]
                .align(&self.text)
                .is_forward()
                .unwrap_or(true)
            {
                Selection {
                    anchor: Idx(0),
                    cursor: Idx(self.text.len_chars()),
                }
            } else {
                Selection {
                    cursor: Idx(0),
                    anchor: Idx(self.text.len_chars()),
                }
            },
            &self.text,
        )];
    }
}

/// The editor state
#[derive(Clone)]
pub struct State {
    quit: bool,
    mode: Mode,
    buffer: Buffer,
    yanked: Vec<Rope>,
}

impl Default for State {
    fn default() -> Self {
        State {
            quit: false,
            mode: Mode::default(),
            buffer: default(),
            yanked: vec![],
        }
    }
}

/// The editor instance
///
/// Screen drawing + state handling
struct Breeze {
    state: State,
    screen: AlternateScreen<termion::raw::RawTerminal<std::io::Stdout>>,
    display_cols: usize,
    display_rows: usize,
    prev_start_line: usize,
    window_margin: usize,
}

impl Breeze {
    fn init() -> Result<Self> {
        let screen = AlternateScreen::from(std::io::stdout().into_raw_mode().unwrap());

        let mut breeze = Breeze {
            state: default(),
            display_cols: 0,
            screen,
            display_rows: 0,
            prev_start_line: 0,
            window_margin: 0,
        };
        breeze.fix_size()?;

        Ok(breeze)
    }

    fn fix_size(&mut self) -> Result<()> {
        let (cols, rows) = termion::terminal_size()?;
        self.display_cols = cols as usize;
        self.display_rows = rows as usize;
        self.window_margin = self.display_rows / 5;
        Ok(())
    }

    fn open(&mut self, path: &Path) -> Result<()> {
        let text = Rope::from_reader(std::io::BufReader::new(std::fs::File::open(path)?))?;
        self.state.buffer = Buffer::from_text(text);
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        self.draw_buffer()?;

        let stdin = std::io::stdin();
        for e in stdin.events() {
            match e {
                Ok(Event::Key(key)) => {
                    self.state = self.state.mode.handle(self.state.clone(), key);
                }
                Ok(Event::Unsupported(_u)) => {
                    eprintln!("{:?}", _u);
                    self.fix_size()?;
                }
                Ok(Event::Mouse(_)) => {
                    // no animal support yet
                }
                Err(e) => panic!("{}", e),
            }

            if self.state.quit {
                return Ok(());
            }
            self.draw_buffer()?;
        }
        Ok(())
    }

    fn draw_buffer(&mut self) -> Result<()> {
        let buf = self.draw_to_buf();
        self.screen.write_all(&buf)?;
        self.screen.flush()?;
        Ok(())
    }

    fn draw_to_buf(&mut self) -> Vec<u8> {
        let mut buf = CachingAnsciWriter::default();

        buf.reset_color().unwrap();

        write!(&mut buf, "{}", termion::clear::All).unwrap();
        let window_height = self.display_rows - 1;
        let cursor_pos = self.state.buffer.cursor_pos();
        let max_start_line = cursor_pos.line.saturating_sub(self.window_margin);
        let min_start_line = cursor_pos
            .line
            .saturating_add(self.window_margin)
            .saturating_sub(window_height);
        debug_assert!(min_start_line <= max_start_line);

        if max_start_line < self.prev_start_line {
            self.prev_start_line = max_start_line;
        }
        if self.prev_start_line < min_start_line {
            self.prev_start_line = min_start_line;
        }

        let start_line = min(
            self.prev_start_line,
            self.state.buffer.text.len_lines() - window_height,
        );
        let end_line = start_line + window_height;

        let mut ch_idx = Coord {
            line: start_line,
            column: 0,
        }
        .to_idx(&self.state.buffer.text)
        .0;

        for (visual_line_i, line_i) in (start_line..end_line).enumerate() {
            if line_i >= self.state.buffer.text.len_lines() {
                break;
            }

            let line = self.state.buffer.text.line(line_i);

            write!(
                &mut buf,
                "{}",
                termion::cursor::Goto(1, visual_line_i as u16 + 1)
            )
            .unwrap();
            for (char_i, ch) in line.chars().enumerate().take(self.display_cols) {
                let in_selection = self.state.buffer.is_idx_selected(Idx(ch_idx + char_i));
                let ch = if ch == '\n' {
                    if in_selection {
                        Some('·')
                    } else {
                        None
                    }
                } else {
                    Some(ch)
                };

                if let Some(ch) = ch {
                    if in_selection {
                        buf.change_color(color::AnsiValue(16), color::AnsiValue(4))
                            .unwrap();
                        write!(&mut buf, "{}", ch).unwrap();
                    } else {
                        buf.reset_color().unwrap();
                        write!(&mut buf, "{}", ch).unwrap();
                    }
                }
            }
            ch_idx += line.len_chars();
        }

        // status
        buf.reset_color().unwrap();
        write!(
            &mut buf,
            "{}{} {}",
            termion::cursor::Goto(1, self.display_rows as u16),
            self.state.mode.name(),
            self.state.mode.num_prefix_str(),
        )
        .unwrap();

        // cursor
        write!(
            &mut buf,
            "\x1b[6 q{}{}",
            termion::cursor::Goto(
                cursor_pos.column as u16 + 1,
                (cursor_pos.line - start_line) as u16 + 1
            ),
            termion::cursor::Show,
        )
        .unwrap();
        buf.into_vec()
    }
}

fn main() -> Result<()> {
    let opt = opts::Opts::from_args();
    let mut brz = Breeze::init()?;

    if let Some(path) = opt.input {
        brz.open(&path)?;
    }

    brz.run()?;
    Ok(())
}
