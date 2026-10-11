// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Kin logo, printed when a person starts working with Kin.
//!
//! Three moments print it: the start of an interactive `kin setup`, the start
//! of a human-mode `kin init`, and a bare `kin`, whose help screen is the first
//! thing a new user reads. Nothing else does. `kin --help` and every
//! subcommand's help stay plain text, because scripts and snapshot tests read
//! them, and the smaller mark in [`crate::mark`] already covers
//! `kin --version`, the `kin init` result and the `kin doctor` header.
//!
//! The logo is the brand lockup: the Kin mark beside the KIN wordmark, with
//! the brand line under the wordmark, nine rows and at most [`WIDTH`]
//! columns. The mark is `docs/assets/kin-icon.svg` sampled onto a grid of
//! square pixels: each terminal row carries two pixel rows through the
//! half-block glyphs, and the mark is eighteen of them tall. Its two bands, an
//! arm and a leg, keep the brand's split as a one-pixel channel, and no cell
//! holds both, so they stay two shapes even without colour. The wordmark
//! follows the brand lockup's proportions: its cap height is two thirds of the
//! mark, its strokes are two pixels, and its K's arm meets the stem low, with
//! the leg springing from the arm, as the brand K does.
//!
//! A terminal narrower than [`FULL_MIN_COLUMNS`] gets the compact lockup
//! instead: the mark drawn ten pixels tall beside a letter-spaced `K  I  N`,
//! with the brand line under it. A terminal too narrow even for that gets no
//! logo.
//!
//! Words keep the terminal's own foreground, so they read on a light theme and
//! a dark one alike. The wordmark is never painted, the compact `K  I  N` is
//! bold and the brand line is faint. Only the mark carries colour.
//!
//! It goes to stdout, only on a terminal, and at most once per process.
//! Nothing is printed when the caller's output is for a program (`--json`, a
//! non-interactive setup), when `CI` is set, when `KIN_NO_BANNER` is truthy,
//! or when the terminal is too narrow to hold it. `NO_COLOR` and `TERM=dumb`
//! take the colour away and leave the logo. The colour depth is
//! [`crate::mark`]'s decision: the brand gradients on a truecolor terminal,
//! one 256-colour index per band, or ANSI magenta and bright blue.
//!
//! The half blocks are drawn only where the terminal reliably reads UTF-8: a
//! UTF-8 locale, or on Windows, Windows Terminal or a console on the UTF-8
//! code page. Anywhere else the logo is ASCII, with the arm hatched in `/` and
//! the leg in `\`, the directions the two bands run.
//!
//! Terminal.app, the terminal a new Mac opens, draws block glyphs from its
//! font, one em tall in a taller line, so half blocks print there as tiles
//! with a seam under every row. With colour on it gets the logo in painted
//! cells instead (see [`CELLS`]): each inked cell's background takes the ink,
//! which fills the cell edge to edge, at one pixel row a cell.

use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::mark::{Glyphs, Paint};

/// The logo in pixels, two to a terminal row.
///
/// `A` is the mark's arm, `L` its leg, `W` the wordmark and `.` background.
/// The mark fills the first thirteen columns and is the brand mark sampled at
/// this size: a pixel is inked when the mark covers half of it or more. The
/// arm is a 45 degree band with a flat top and a flat left side that ends in a
/// point, and the leg a parallel band, cut square at its top and flat at its
/// foot. The one-pixel channel between the arm's point and the leg's cut is the
/// split.
///
/// The wordmark starts at [`WORDMARK_COLUMN`], two pixels down, and is twelve
/// pixels tall, two thirds of the mark as in the brand lockup. It sits on whole
/// terminal rows, so every stem is solid blocks with square ends. The K's arm
/// runs down to the stem and its leg springs from the arm, and the N's diagonal
/// moves one column every two pixel rows. The letters are spaced as the brand
/// lockup spaces them, a little wider between the I and the N.
const PIXELS: [&str; 18] = [
    "....AAAAAAA.........................................",
    "...AAAAAAAA.........................................",
    "..AAAAAAAA...........WW......WW....WW.....WWWW....WW",
    ".AAAAAAAA............WW.....WW.....WW.....WWWW....WW",
    "AAAAAAAA.............WW....WW......WW.....WW.WW...WW",
    "AAAAAAA..............WW...WW.......WW.....WW.WW...WW",
    "AAAAAA...............WW..WW........WW.....WW..WW..WW",
    "AAAAA................WW.WWWW.......WW.....WW..WW..WW",
    "AAAA.................WWWW..WW......WW.....WW...WW.WW",
    "AAA..................WWW...WW......WW.....WW...WW.WW",
    "AA..LLL..............WW.....WW.....WW.....WW....WWWW",
    "...LLLLL.............WW.....WW.....WW.....WW....WWWW",
    "..LLLLLLL............WW......WW....WW.....WW.....WWW",
    ".LLLLLLLLL...........WW......WW....WW.....WW.....WWW",
    ".LLLLLLLLLL.........................................",
    "..LLLLLLLLLL........................................",
    "...LLLLLLLLLL.......................................",
    "....LLLLLLLLL.......................................",
];

/// The logo in ASCII, one string per terminal row.
///
/// Left of [`WORDMARK_COLUMN`], `/` is the arm and `\` the leg, inked in
/// exactly the cells the half blocks ink. From that column on, every glyph is
/// the wordmark: `#` for the stems, and the K and N diagonals hatched one
/// stroke wide in the directions they run.
const ASCII_ROWS: [&str; ROWS] = [
    r"   ////////",
    r" /////////           ##     //     ##     ##\\    ##",
    r"////////             ##   //       ##     ## \\   ##",
    r"//////               ## //\\       ##     ##  \\  ##",
    r"////                 ##//  \\      ##     ##   \\ ##",
    r"// \\\\\             ##     \\     ##     ##    \\##",
    r" \\\\\\\\\           ##      \\    ##     ##     \##",
    r" \\\\\\\\\\\",
    r"   \\\\\\\\\\",
];

/// Terminal rows the logo takes.
const ROWS: usize = PIXELS.len() / 2;

/// The column the wordmark starts at, which is the K's stem, and the column
/// the brand line under it starts at.
const WORDMARK_COLUMN: usize = 21;

/// Blank columns in front of the logo, the indent every first-run line has.
const MARGIN: usize = 2;

/// The brand line, verbatim from the brand canon.
const BRAND_LINE: &str = "A new foundation for code.";

/// The row the brand line sits on: the last, under the wordmark.
const BRAND_LINE_ROW: usize = ROWS - 1;

/// The compact lockup's mark: the brand mark ten pixels tall, five rows.
///
/// Drawn rather than sampled. At this size a sampled leg rounds into a mound,
/// so each band is kept four pixels wide and stepped one column a row, which
/// is what makes two bands read as the arm and the leg. The arm's point and the
/// leg's cut keep the split.
#[rustfmt::skip]
const COMPACT_PIXELS: [&str; 10] = [
    "..AAAA..",
    ".AAAAA..",
    "AAAA....",
    "AAA.....",
    "AA......",
    "A.......",
    "..LL....",
    ".LLLL...",
    "..LLLL..",
    "...LLLL.",
];

/// The compact mark in ASCII, inked in the cells its half blocks ink.
#[rustfmt::skip]
const COMPACT_ASCII_ROWS: [&str; COMPACT_ROWS] = [
    r" /////",
    r"////",
    r"//",
    r" \\\\",
    r"  \\\\\",
];

/// The full lockup at whole-cell resolution, for a terminal that draws block
/// glyphs from its font, one character per cell.
///
/// Such a terminal, Terminal.app the one most people meet, draws a block glyph
/// one em tall in a line taller than that, so half-block art prints as tiles
/// with a seam under every row. A cell's background fills the whole cell, so
/// this drawing paints each inked cell's background instead and draws no glyph.
/// A cell is one pixel wide and two tall, half the half blocks' vertical
/// resolution: the arm and the leg step two columns a row, and the split
/// stays a one-cell channel. The wordmark keeps its stems two columns wide and
/// takes the terminal's own foreground through reverse video.
const CELLS: [&str; ROWS] = [
    "...AAAAAAAA.........................................",
    ".AAAAAAAA............WW......WW....WW.....WWWW....WW",
    "AAAAAAA..............WW....WW......WW.....WW.WW...WW",
    "AAAAA................WW..WW........WW.....WW..WW..WW",
    "AAA..................WWWW..WW......WW.....WW...WW.WW",
    "A...LLLL.............WW.....WW.....WW.....WW....WWWW",
    "..LLLLLLLL...........WW......WW....WW.....WW.....WWW",
    "..LLLLLLLLLL........................................",
    "....LLLLLLLLL.......................................",
];

/// The compact mark at whole-cell resolution, six rows.
///
/// Each band is a parallelogram: the arm four rows stepping down to the left,
/// the leg two rows stepping down to the right, a clear row between the arm's
/// point and the leg.
#[rustfmt::skip]
const COMPACT_CELLS: [&str; 6] = [
    "..AAAAA..",
    "AAAAA....",
    "AAA......",
    "A........",
    "..LLLLL..",
    "...LLLLLL",
];

/// Terminal rows the compact lockup takes, drawn in half blocks.
const COMPACT_ROWS: usize = COMPACT_PIXELS.len() / 2;

/// The column the compact lockup's words start at, counted from the mark's
/// first column.
const COMPACT_TEXT_COLUMN: usize = 11;

/// The compact lockup's wordmark: the three letters, set bold and spaced.
const SET_WORDMARK: &str = "K  I  N";

/// The row the compact wordmark sits on, beside the arm, in each compact
/// drawing. The brand line sits on the row under it.
const COMPACT_WORDMARK_ROW: usize = 1;
const COMPACT_CELLS_WORDMARK_ROW: usize = 2;

const fn max(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

/// The widest line the full lockup prints, margin included: a wordmark row,
/// or the brand line if it were ever the longer.
///
/// A terminal narrower than this gets no full lockup rather than a wrapped
/// one.
pub const WIDTH: usize = max(
    MARGIN + PIXELS[0].len(),
    MARGIN + WORDMARK_COLUMN + BRAND_LINE.len(),
);

// The lockup's width is a budget, and a change that widens it past that budget
// fails the build rather than a reader's terminal.
const _: () = assert!(WIDTH <= 54, "the logo is wider than 54 columns");

/// Below this width the compact lockup prints instead of the full one.
///
/// The width the first run treats as narrow. The full lockup fits in fewer
/// columns, but beside rows that dropped their timing column to fit, it would
/// be the widest thing on the screen.
const FULL_MIN_COLUMNS: usize = 60;

const _: () = assert!(
    WIDTH <= FULL_MIN_COLUMNS,
    "the full lockup is chosen on terminals too narrow to hold it"
);

/// The widest line the compact lockup prints: the brand line beside the
/// compact mark, margin included. Below this width no logo prints.
const COMPACT_WIDTH: usize = MARGIN + COMPACT_TEXT_COLUMN + BRAND_LINE.len();

/// A linear gradient's two end points, in a drawing's pixels.
type Gradient = [(f32, f32); 2];

/// How a drawing's pixel rows become terminal rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    /// Two pixel rows a terminal row, drawn with the half-block glyphs in the
    /// ink's foreground colour. Crisp on a terminal that draws block glyphs
    /// itself, which most do.
    HalfBlocks,
    /// One pixel row a terminal row, each inked cell a space painted with the
    /// ink's background colour. For a terminal whose block glyphs come from
    /// its font and leave seams; see [`CELLS`].
    Cells,
}

/// One drawing of the mark: its pixels, how they fill the terminal, its ASCII
/// rows, where its words go, and the brand gradients placed on its pixels.
struct Art {
    pixels: &'static [&'static str],
    fill: Fill,
    /// The ASCII rows, for a half-block drawing. A cell drawing needs none: it
    /// is spaces and escapes, which any locale carries.
    ascii: &'static [&'static str],
    /// The first column of the wordmark, for a drawing that carries one.
    wordmark_column: Option<usize>,
    /// The row a compact drawing sets `K  I  N` on.
    wordmark_row: usize,
    arm: Gradient,
    leg: Gradient,
}

impl Art {
    /// Terminal rows the drawing takes.
    fn rows(&self) -> usize {
        match self.fill {
            Fill::HalfBlocks => self.pixels.len() / 2,
            Fill::Cells => self.pixels.len(),
        }
    }
}

/// The full lockup's drawing.
const FULL: Art = Art {
    pixels: &PIXELS,
    fill: Fill::HalfBlocks,
    ascii: &ASCII_ROWS,
    wordmark_column: Some(WORDMARK_COLUMN),
    wordmark_row: 0,
    arm: brand_gradient(ARM_STOPS, 18.0),
    leg: brand_gradient(LEG_STOPS, 18.0),
};

/// The full lockup in cells. Nine rows of cells two pixels tall are the same
/// eighteen pixels, so the gradients are the half-block drawing's.
const FULL_CELLS: Art = Art {
    pixels: &CELLS,
    fill: Fill::Cells,
    ascii: &[],
    wordmark_column: Some(WORDMARK_COLUMN),
    wordmark_row: 0,
    arm: brand_gradient(ARM_STOPS, 18.0),
    leg: brand_gradient(LEG_STOPS, 18.0),
};

/// The compact lockup's mark.
const COMPACT: Art = Art {
    pixels: &COMPACT_PIXELS,
    fill: Fill::HalfBlocks,
    ascii: &COMPACT_ASCII_ROWS,
    wordmark_column: None,
    wordmark_row: COMPACT_WORDMARK_ROW,
    arm: brand_gradient(ARM_STOPS, 10.0),
    leg: brand_gradient(LEG_STOPS, 10.0),
};

/// The compact mark in cells, twelve pixels tall.
const COMPACT_IN_CELLS: Art = Art {
    pixels: &COMPACT_CELLS,
    fill: Fill::Cells,
    ascii: &[],
    wordmark_column: None,
    wordmark_row: COMPACT_CELLS_WORDMARK_ROW,
    arm: brand_gradient(ARM_STOPS, 12.0),
    leg: brand_gradient(LEG_STOPS, 12.0),
};

/// Where `kin-icon.svg` starts and ends each band's gradient, in its own
/// units: the arm from (391, 52) to (105, 338), the leg from (177, 309) to
/// (436, 460).
const ARM_STOPS: Gradient = [(391.0, 52.0), (105.0, 338.0)];
const LEG_STOPS: Gradient = [(177.0, 309.0), (436.0, 460.0)];

/// A gradient from `kin-icon.svg`'s units into the pixels of a mark `height`
/// pixels tall. The mark's top-left corner there is (105, 52) and it is 408.1
/// units tall.
const fn brand_gradient(stops: Gradient, height: f32) -> Gradient {
    let unit = height / 408.1;
    [
        ((stops[0].0 - 105.0) * unit, (stops[0].1 - 52.0) * unit),
        ((stops[1].0 - 105.0) * unit, (stops[1].1 - 52.0) * unit),
    ]
}

/// One 256-colour index per piece: the cube's purple nearest the arm's first
/// stop, and its blue nearest the leg's last.
///
/// A two-step ramp inside each piece read as a crease across it, and the two
/// pieces' average colours land on neighbouring indices that barely tell the
/// arm from the leg, which is the one thing the mark has to show.
const INDEXED_ARM: u8 = 135;
const INDEXED_LEG: u8 = 69;

/// Bold and faint, in the terminal's own foreground. Words are never given a
/// fixed colour, because none reads on both a dark and a light background.
const BOLD: &str = "\u{1b}[1m";
const FAINT: &str = "\u{1b}[2m";

const RESET: &str = "\u{1b}[0m";

/// Reverse video: a cell drawing's wordmark, painted in the terminal's own
/// foreground.
const REVERSE: &str = "\u{1b}[7m";

/// The sixteen-colour arm and leg as backgrounds: magenta and bright blue, as
/// [`crate::mark::BASIC_ARM`] and [`crate::mark::BASIC_LEG`] are foregrounds.
const BASIC_ARM_BACKGROUND: &str = "\u{1b}[45m";
const BASIC_LEG_BACKGROUND: &str = "\u{1b}[104m";

/// Set once the logo has printed, so a process prints it at most once.
static PRINTED: AtomicBool = AtomicBool::new(false);

/// Print the logo on stdout when this is a moment for it, and say whether it
/// printed.
///
/// `human_mode` is the caller's word that a person reads this command's output
/// as it runs. Pass `false` for `--json`, for a non-interactive setup, and for
/// any other output a program reads. Everything else, the terminal, `CI`,
/// `KIN_NO_BANNER`, the width and the colour depth, is checked here.
///
/// The logo brings a blank line above and below it, so a caller that opens
/// with a blank line of its own can drop that line when this returns `true`.
pub fn print_once(human_mode: bool) -> bool {
    print_with(human_mode, None)
}

/// [`print_once`], for a caller that needs `rows_after` more rows on screen
/// together with the logo, such as `kin setup`'s first question.
///
/// A terminal too short for the full lockup and those rows gets the compact
/// lockup, so what follows is never pushed off the top by the art above it.
pub fn print_once_leaving(human_mode: bool, rows_after: usize) -> bool {
    print_with(human_mode, Some(rows_after))
}

/// Whether the full lockup, its two blank lines and `rows_after` more rows fit
/// a terminal `rows` tall.
fn full_lockup_fits(rows: Option<usize>, rows_after: Option<usize>) -> bool {
    match (rows, rows_after) {
        (Some(rows), Some(after)) => ROWS + 2 + after <= rows,
        _ => true,
    }
}

fn print_with(human_mode: bool, rows_after: Option<usize>) -> bool {
    if !human_mode || PRINTED.load(Ordering::SeqCst) {
        return false;
    }
    let read =
        |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    let surroundings = Surroundings {
        stdout_is_terminal: std::io::stdout().is_terminal(),
        columns: crate::mark::terminal_columns(),
        windows: cfg!(windows),
        utf8_console: console_output_is_utf8(),
        var: &read,
    };
    let Some(mut style) = plan(&surroundings, human_mode) else {
        return false;
    };
    let rows = console::Term::stdout()
        .size_checked()
        .map(|(rows, _)| usize::from(rows));
    if style.lockup == Lockup::Full && !full_lockup_fits(rows, rows_after) {
        style.lockup = Lockup::Compact;
    }
    if !claim(&PRINTED) {
        return false;
    }
    print!("{}", framed(&render(style)));
    let _ = std::io::stdout().flush();
    true
}

/// The logo's lines with a blank line above and below, as one write.
fn framed(lines: &[String]) -> String {
    let mut block = String::from("\n");
    for line in lines {
        block.push_str(line);
        block.push('\n');
    }
    block.push('\n');
    block
}

/// Take the once-per-process slot, `true` only for the first caller.
fn claim(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

/// Which lockup the terminal's width allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lockup {
    /// The mark beside the drawn wordmark, the brand lockup.
    Full,
    /// The small mark beside set type, for a narrow terminal.
    Compact,
}

/// How the logo is drawn once it is decided that it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Style {
    lockup: Lockup,
    glyphs: Glyphs,
    paint: Paint,
    fill: Fill,
}

/// What the decision reads, gathered in one place.
///
/// The decision is a pure function of this, so a test drives every branch
/// through `var` without setting a process environment variable, which no test
/// in this binary can do without racing the others.
struct Surroundings<'a> {
    stdout_is_terminal: bool,
    columns: Option<usize>,
    windows: bool,
    utf8_console: bool,
    var: &'a dyn Fn(&str) -> Option<String>,
}

impl Surroundings<'_> {
    /// The variable's value, empty included.
    fn raw(&self, name: &str) -> Option<String> {
        (self.var)(name)
    }

    /// The variable's value, with an empty value read as unset.
    fn set(&self, name: &str) -> Option<String> {
        self.raw(name).filter(|value| !value.is_empty())
    }
}

/// How to draw the logo here, or `None` when it should not be drawn.
fn plan(surroundings: &Surroundings<'_>, human_mode: bool) -> Option<Style> {
    if !human_mode || !surroundings.stdout_is_terminal {
        return None;
    }
    // A CI log is read after the fact, where nine rows of art are noise.
    // `CI=false` is how some runners say they are not one, so a falsy value
    // does not count.
    if surroundings
        .set("CI")
        .is_some_and(|value| !is_false(&value))
    {
        return None;
    }
    if surroundings
        .set("KIN_NO_BANNER")
        .is_some_and(|value| is_true(&value))
    {
        return None;
    }
    // An unknown width is given the benefit of the doubt.
    let lockup = match surroundings.columns {
        Some(columns) if columns < COMPACT_WIDTH => return None,
        Some(columns) if columns < FULL_MIN_COLUMNS => Lockup::Compact,
        _ => Lockup::Full,
    };
    let term = surroundings.raw("TERM");
    let windows_terminal = surroundings.set("WT_SESSION").is_some();
    let colour = crate::output_style::wanted(
        surroundings.stdout_is_terminal,
        surroundings.raw("NO_COLOR").is_some(),
        term.as_deref(),
    );
    let paint = crate::mark::paint_for(
        colour,
        surroundings.set("COLORTERM").as_deref(),
        term.as_deref(),
        windows_terminal,
    );
    let locale = [
        surroundings.raw("LC_ALL"),
        surroundings.raw("LC_CTYPE"),
        surroundings.raw("LANG"),
    ];
    let glyphs = glyphs_for(
        &locale,
        surroundings.windows,
        windows_terminal,
        surroundings.utf8_console,
    );
    // Terminal.app draws block glyphs from the font, one em tall in a taller
    // line, so half blocks print as tiles. Painted cells fill the whole cell,
    // and need colour to paint with.
    let fill = if paint != Paint::None
        && surroundings.set("TERM_PROGRAM").as_deref() == Some("Apple_Terminal")
    {
        Fill::Cells
    } else {
        Fill::HalfBlocks
    };
    Some(Style {
        lockup,
        glyphs,
        paint,
        fill,
    })
}

/// Whether this terminal reliably reads the half blocks.
///
/// A locale variable that is set decides, in POSIX precedence, through
/// [`crate::mark::glyphs_for_locale`]. With none set, a Unix process is in the
/// C locale, which is ASCII. Windows sets none of them, and there Windows
/// Terminal and a console on the UTF-8 code page read the blocks, while the
/// legacy code pages print them as question marks.
///
/// Stricter than the small mark's rule, which reads an unset locale as UTF-8:
/// four glyphs that come out wrong cost little, and a logo of them costs the
/// first thing a new user sees.
fn glyphs_for(
    locale: &[Option<String>],
    windows: bool,
    windows_terminal: bool,
    utf8_console: bool,
) -> Glyphs {
    let any_locale = locale
        .iter()
        .any(|value| value.as_deref().is_some_and(|value| !value.is_empty()));
    if any_locale {
        return crate::mark::glyphs_for_locale(locale);
    }
    if windows && (windows_terminal || utf8_console) {
        Glyphs::Unicode
    } else {
        Glyphs::Ascii
    }
}

/// Whether this process's console writes UTF-8, the code page `chcp 65001`
/// selects.
#[cfg(windows)]
fn console_output_is_utf8() -> bool {
    const CP_UTF8: u32 = 65001;
    // SAFETY: `GetConsoleOutputCP` takes no arguments and only reads the
    // calling process's console state. It returns 0 when there is no console.
    unsafe { windows_sys::Win32::System::Console::GetConsoleOutputCP() == CP_UTF8 }
}

#[cfg(not(windows))]
fn console_output_is_utf8() -> bool {
    false
}

/// `1`, `true`, `yes` or `on`, the environment registry's truthy values.
fn is_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `0`, `false`, `no` or `off`, the environment registry's falsy values.
fn is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Which part of the logo a glyph belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    Arm,
    Leg,
    Wordmark,
}

fn pixel_piece(pixel: u8) -> Option<Piece> {
    match pixel {
        b'A' => Some(Piece::Arm),
        b'L' => Some(Piece::Leg),
        b'W' => Some(Piece::Wordmark),
        _ => None,
    }
}

/// One terminal cell: its glyph, and for an inked cell the piece and the
/// pixel-space point its colour is sampled at.
#[derive(Clone, Copy, Debug)]
struct Cell {
    glyph: char,
    ink: Option<(Piece, f32, f32)>,
}

/// One row of a half-block drawing, built from the two pixel rows it carries.
///
/// A half block takes its colour at the centre of the pixel it draws, and a
/// full block at the centre of the cell.
fn unicode_row(art: &Art, row: usize) -> Vec<Cell> {
    let top = art.pixels[2 * row].as_bytes();
    let bottom = art.pixels[2 * row + 1].as_bytes();
    let y = (2 * row) as f32;
    top.iter()
        .zip(bottom)
        .enumerate()
        .map(|(column, (&upper, &lower))| {
            let x = column as f32 + 0.5;
            let (glyph, ink) = match (pixel_piece(upper), pixel_piece(lower)) {
                (Some(piece), Some(_)) => ('\u{2588}', Some((piece, x, y + 1.0))),
                (Some(piece), None) => ('\u{2580}', Some((piece, x, y + 0.5))),
                (None, Some(piece)) => ('\u{2584}', Some((piece, x, y + 1.5))),
                (None, None) => (' ', None),
            };
            Cell { glyph, ink }
        })
        .collect()
}

/// One row of an ASCII drawing.
fn ascii_row(art: &Art, row: usize) -> Vec<Cell> {
    let y = (2 * row) as f32 + 1.0;
    art.ascii[row]
        .chars()
        .enumerate()
        .map(|(column, glyph)| {
            let in_wordmark = art
                .wordmark_column
                .is_some_and(|wordmark| column >= wordmark);
            let piece = match glyph {
                ' ' => None,
                _ if in_wordmark => Some(Piece::Wordmark),
                '/' => Some(Piece::Arm),
                '\\' => Some(Piece::Leg),
                _ => Some(Piece::Wordmark),
            };
            Cell {
                glyph,
                ink: piece.map(|piece| (piece, column as f32 + 0.5, y)),
            }
        })
        .collect()
}

/// One row of a cell drawing: a space per cell, inked cells carrying the
/// piece and the centre of the cell, in pixels, that their colour is sampled
/// at.
fn cell_row(art: &Art, row: usize) -> Vec<Cell> {
    art.pixels[row]
        .bytes()
        .enumerate()
        .map(|(column, pixel)| Cell {
            glyph: ' ',
            ink: pixel_piece(pixel)
                .map(|piece| (piece, column as f32 + 0.5, (2 * row) as f32 + 1.0)),
        })
        .collect()
}

/// A drawing's row, painted, with nothing after its last inked cell, and the
/// number of columns it takes.
fn drawn_row(art: &Art, glyphs: Glyphs, paint: Paint, row: usize) -> (String, usize) {
    let cells = match (art.fill, glyphs) {
        (Fill::Cells, _) => cell_row(art, row),
        (Fill::HalfBlocks, Glyphs::Unicode) => unicode_row(art, row),
        (Fill::HalfBlocks, Glyphs::Ascii) => ascii_row(art, row),
    };
    let drawn = cells
        .iter()
        .rposition(|cell| cell.ink.is_some())
        .map_or(0, |last| last + 1);
    (paint_cells(art, &cells[..drawn], paint), drawn)
}

/// The logo's lines, without the blank lines around it.
///
/// A cell drawing needs colour to paint with, so without it the half-block
/// drawing prints whatever the fill.
fn render(style: Style) -> Vec<String> {
    let cells = style.fill == Fill::Cells && style.paint != Paint::None;
    match (style.lockup, cells) {
        (Lockup::Full, false) => render_full(&FULL, style.glyphs, style.paint),
        (Lockup::Full, true) => render_full(&FULL_CELLS, style.glyphs, style.paint),
        (Lockup::Compact, false) => render_compact(&COMPACT, style.glyphs, style.paint),
        (Lockup::Compact, true) => render_compact(&COMPACT_IN_CELLS, style.glyphs, style.paint),
    }
}

/// The brand lockup: the mark beside the drawn wordmark, and the brand line
/// under the wordmark on the mark's last row, starting at the K's stem.
fn render_full(art: &Art, glyphs: Glyphs, paint: Paint) -> Vec<String> {
    (0..art.rows())
        .map(|row| {
            let (painted, drawn) = drawn_row(art, glyphs, paint, row);
            let mut line = " ".repeat(MARGIN);
            line.push_str(&painted);
            if row == BRAND_LINE_ROW {
                line.push_str(&" ".repeat(WORDMARK_COLUMN.saturating_sub(drawn)));
                line.push_str(&words(paint, FAINT, BRAND_LINE));
            }
            line
        })
        .collect()
}

/// The compact lockup: the compact mark beside `K  I  N` in bold, with the
/// brand line under it in faint.
fn render_compact(art: &Art, glyphs: Glyphs, paint: Paint) -> Vec<String> {
    (0..art.rows())
        .map(|row| {
            let (painted, drawn) = drawn_row(art, glyphs, paint, row);
            let mut line = " ".repeat(MARGIN);
            line.push_str(&painted);
            let text = if row == art.wordmark_row {
                Some(words(paint, BOLD, SET_WORDMARK))
            } else if row == art.wordmark_row + 1 {
                Some(words(paint, FAINT, BRAND_LINE))
            } else {
                None
            };
            if let Some(text) = text {
                line.push_str(&" ".repeat(COMPACT_TEXT_COLUMN.saturating_sub(drawn)));
                line.push_str(&text);
            }
            line
        })
        .collect()
}

/// `text` in bold or faint when this terminal takes escapes, and plain when it
/// does not.
fn words(paint: Paint, weight: &str, text: &str) -> String {
    match paint {
        Paint::None => text.to_string(),
        _ => format!("{weight}{text}{RESET}"),
    }
}

/// A row's glyphs with their colour escapes.
///
/// An escape is written only where the colour changes, and a reset only where
/// colour stops, so the 256- and 16-colour rows carry one escape per run
/// rather than one per cell. The wordmark is never given a colour: it takes
/// the terminal's own foreground, which is light on a dark theme and dark on a
/// light one, as the brand's two lockups are. A cell drawing paints
/// backgrounds, and reaches that foreground through reverse video.
fn paint_cells(art: &Art, cells: &[Cell], paint: Paint) -> String {
    let mut out = String::new();
    let mut open: Option<String> = None;
    for cell in cells {
        let escape = cell.ink.and_then(|(piece, x, y)| match art.fill {
            Fill::HalfBlocks => ink_escape(art, paint, piece, x, y),
            Fill::Cells => cell_escape(art, paint, piece, x, y),
        });
        if escape != open {
            // Reverse video outlives a colour change, so a cell drawing
            // closes each run before the next opens.
            if art.fill == Fill::Cells && open.is_some() && escape.is_some() {
                out.push_str(RESET);
            }
            match &escape {
                Some(escape) => out.push_str(escape),
                None => out.push_str(RESET),
            }
            open = escape;
        }
        out.push(cell.glyph);
    }
    if open.is_some() {
        out.push_str(RESET);
    }
    out
}

/// The escape that colours one inked cell of a drawing, or `None` when it
/// stays uncoloured.
fn ink_escape(art: &Art, paint: Paint, piece: Piece, x: f32, y: f32) -> Option<String> {
    let stops = &crate::mark::TRUECOLOR_STOPS;
    match (paint, piece) {
        (Paint::None, _) | (_, Piece::Wordmark) => None,
        (Paint::Truecolor, Piece::Arm) => {
            Some(truecolor(mix(stops[0], stops[1], along(art.arm, x, y))))
        }
        (Paint::Truecolor, Piece::Leg) => {
            Some(truecolor(mix(stops[2], stops[3], along(art.leg, x, y))))
        }
        (Paint::Indexed, Piece::Arm) => Some(format!("\u{1b}[38;5;{INDEXED_ARM}m")),
        (Paint::Indexed, Piece::Leg) => Some(format!("\u{1b}[38;5;{INDEXED_LEG}m")),
        (Paint::Basic, Piece::Arm) => Some(crate::mark::BASIC_ARM.to_string()),
        (Paint::Basic, Piece::Leg) => Some(crate::mark::BASIC_LEG.to_string()),
    }
}

/// The escape that paints one inked cell of a cell drawing: the ink as the
/// cell's background, or reverse video for the wordmark, so it takes the
/// terminal's own foreground. `None` when this terminal takes no colour.
fn cell_escape(art: &Art, paint: Paint, piece: Piece, x: f32, y: f32) -> Option<String> {
    let stops = &crate::mark::TRUECOLOR_STOPS;
    match (paint, piece) {
        (Paint::None, _) => None,
        (_, Piece::Wordmark) => Some(REVERSE.to_string()),
        (Paint::Truecolor, Piece::Arm) => {
            Some(background(mix(stops[0], stops[1], along(art.arm, x, y))))
        }
        (Paint::Truecolor, Piece::Leg) => {
            Some(background(mix(stops[2], stops[3], along(art.leg, x, y))))
        }
        (Paint::Indexed, Piece::Arm) => Some(format!("\u{1b}[48;5;{INDEXED_ARM}m")),
        (Paint::Indexed, Piece::Leg) => Some(format!("\u{1b}[48;5;{INDEXED_LEG}m")),
        (Paint::Basic, Piece::Arm) => Some(BASIC_ARM_BACKGROUND.to_string()),
        (Paint::Basic, Piece::Leg) => Some(BASIC_LEG_BACKGROUND.to_string()),
    }
}

/// How far along a linear gradient a point sits, from 0 at its start to 1 at
/// its end, as SVG measures it: by projection onto the gradient's vector.
fn along(gradient: Gradient, x: f32, y: f32) -> f32 {
    let [(x0, y0), (x1, y1)] = gradient;
    let (dx, dy) = (x1 - x0, y1 - y0);
    (((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0)
}

/// The colour `t` of the way from one stop to the other, in sRGB as SVG
/// interpolates by default.
fn mix(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let channel = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

fn truecolor((r, g, b): (u8, u8, u8)) -> String {
    format!("\u{1b}[38;2;{r};{g};{b}m")
}

fn background((r, g, b): (u8, u8, u8)) -> String {
    format!("\u{1b}[48;2;{r};{g};{b}m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Every way the logo can be drawn: each lockup, each glyph set, each
    /// colour depth.
    fn all_styles() -> Vec<Style> {
        let mut styles = Vec::new();
        for lockup in [Lockup::Full, Lockup::Compact] {
            for glyphs in [Glyphs::Unicode, Glyphs::Ascii] {
                for paint in [Paint::Truecolor, Paint::Indexed, Paint::Basic, Paint::None] {
                    for fill in [Fill::HalfBlocks, Fill::Cells] {
                        styles.push(Style {
                            lockup,
                            glyphs,
                            paint,
                            fill,
                        });
                    }
                }
            }
        }
        styles
    }

    fn full(glyphs: Glyphs, paint: Paint) -> Style {
        Style {
            lockup: Lockup::Full,
            glyphs,
            paint,
            fill: Fill::HalfBlocks,
        }
    }

    fn compact(glyphs: Glyphs, paint: Paint) -> Style {
        Style {
            lockup: Lockup::Compact,
            glyphs,
            paint,
            fill: Fill::HalfBlocks,
        }
    }

    fn in_cells(lockup: Lockup, paint: Paint) -> Style {
        Style {
            lockup,
            glyphs: Glyphs::Unicode,
            paint,
            fill: Fill::Cells,
        }
    }

    /// Every drawing, half blocks and cells.
    const ARTS: [&Art; 4] = [&FULL, &COMPACT, &FULL_CELLS, &COMPACT_IN_CELLS];

    /// An ordinary truecolor UTF-8 terminal on a Unix machine, with the
    /// variables a test names set on top. A name written `-NAME` unsets it.
    fn decide(vars: &[(&str, &str)]) -> Option<Style> {
        decide_with(true, Some(120), false, false, vars)
    }

    fn decide_with(
        stdout_is_terminal: bool,
        columns: Option<usize>,
        windows: bool,
        utf8_console: bool,
        vars: &[(&str, &str)],
    ) -> Option<Style> {
        let mut env: HashMap<String, String> = [
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("LANG", "en_US.UTF-8"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        for (name, value) in vars {
            match name.strip_prefix('-') {
                Some(unset) => {
                    env.remove(unset);
                }
                None => {
                    env.insert((*name).to_string(), (*value).to_string());
                }
            }
        }
        let read = |name: &str| env.get(name).cloned();
        plan(
            &Surroundings {
                stdout_is_terminal,
                columns,
                windows,
                utf8_console,
                var: &read,
            },
            true,
        )
    }

    /// The logo as it prints with colour off, which is also every other
    /// rendering once its escapes are stripped.
    ///
    /// Written out rather than rebuilt from the pixel rows, so a change to the
    /// shape shows up here as the drawing a reviewer will see.
    #[test]
    fn the_unicode_logo_is_the_mark_beside_the_wordmark() {
        let drawn = render(full(Glyphs::Unicode, Paint::None));
        assert_eq!(
            drawn,
            vec![
                "     ▄███████",
                "   ▄███████▀           ██     ▄█▀    ██     ████    ██",
                "  ███████▀             ██   ▄█▀      ██     ██ ██   ██",
                "  █████▀               ██ ▄██▄       ██     ██  ██  ██",
                "  ███▀                 ███▀  ██      ██     ██   ██ ██",
                "  ▀▀ ▄███▄             ██     ██     ██     ██    ████",
                "   ▄███████▄           ██      ██    ██     ██     ███",
                "   ▀█████████▄",
                "     ▀█████████        A new foundation for code.",
            ]
        );
    }

    #[test]
    fn the_ascii_logo_hatches_the_arm_and_the_leg() {
        let drawn = render(full(Glyphs::Ascii, Paint::None));
        assert_eq!(
            drawn,
            vec![
                r"     ////////",
                r"   /////////           ##     //     ##     ##\\    ##",
                r"  ////////             ##   //       ##     ## \\   ##",
                r"  //////               ## //\\       ##     ##  \\  ##",
                r"  ////                 ##//  \\      ##     ##   \\ ##",
                r"  // \\\\\             ##     \\     ##     ##    \\##",
                r"   \\\\\\\\\           ##      \\    ##     ##     \##",
                r"   \\\\\\\\\\\",
                r"     \\\\\\\\\\        A new foundation for code.",
            ]
        );
        for line in &drawn {
            assert!(line.is_ascii(), "{line:?} is not ASCII");
        }
    }

    /// The compact lockup is the compact mark beside set type.
    #[test]
    fn the_compact_logo_is_the_compact_mark_beside_set_type() {
        assert_eq!(
            render(compact(Glyphs::Unicode, Paint::None)),
            vec![
                "   ▄████",
                "  ███▀       K  I  N",
                "  █▀         A new foundation for code.",
                "   ▄██▄",
                "    ▀███▄",
            ]
        );
        assert_eq!(
            render(compact(Glyphs::Ascii, Paint::None)),
            vec![
                "   /////",
                "  ////       K  I  N",
                "  //         A new foundation for code.",
                r"   \\\\",
                r"    \\\\\",
            ]
        );
    }

    fn inked(line: &str) -> Vec<usize> {
        line.chars()
            .enumerate()
            .filter(|(_, glyph)| *glyph != ' ')
            .map(|(column, _)| column)
            .collect()
    }

    /// The ASCII logo keeps the Unicode one's shape: every row inks the same
    /// columns, so a reader on either sees one lockup.
    #[test]
    fn the_ascii_logo_inks_the_same_columns_as_the_unicode_one() {
        let unicode = render(full(Glyphs::Unicode, Paint::None));
        let ascii = render(full(Glyphs::Ascii, Paint::None));
        for (row, (blocks, hatched)) in unicode.iter().zip(&ascii).enumerate() {
            // Only the mark and the wordmark's stems are compared; the K and
            // N diagonals are hatched one stroke wide rather than drawn.
            let stems = |columns: Vec<usize>| -> Vec<usize> {
                columns
                    .into_iter()
                    .filter(|column| {
                        *column < MARGIN + WORDMARK_COLUMN
                            || [23, 24, 37, 38, 44, 45, 52, 53].contains(column)
                    })
                    .collect()
            };
            assert_eq!(
                stems(inked(blocks)),
                stems(inked(hatched)),
                "row {row}: {blocks:?} against {hatched:?}"
            );
        }
        let unicode = render(compact(Glyphs::Unicode, Paint::None));
        let ascii = render(compact(Glyphs::Ascii, Paint::None));
        for (row, (blocks, hatched)) in unicode.iter().zip(&ascii).enumerate() {
            assert_eq!(
                inked(blocks),
                inked(hatched),
                "compact row {row}: {blocks:?} against {hatched:?}"
            );
        }
    }

    /// The brand line starts at the K's stem, on the mark's last row.
    #[test]
    fn the_brand_line_starts_under_the_k() {
        for glyphs in [Glyphs::Unicode, Glyphs::Ascii] {
            let drawn = render(full(glyphs, Paint::None));
            let stem = drawn[1]
                .chars()
                .skip(MARGIN + WORDMARK_COLUMN - 1)
                .take(3)
                .collect::<String>();
            assert!(
                stem.starts_with(' ') && !stem[1..].starts_with(' '),
                "{glyphs:?}: the K's stem is not at column {}: {:?}",
                MARGIN + WORDMARK_COLUMN,
                drawn[1]
            );
            let last = drawn.last().expect("the logo has rows");
            let at = last
                .find(BRAND_LINE)
                .map(|byte| last[..byte].chars().count());
            assert_eq!(at, Some(MARGIN + WORDMARK_COLUMN), "{glyphs:?}: {last:?}");
            assert_eq!(drawn.len(), ROWS, "{glyphs:?}");
        }
    }

    /// Words keep the terminal's foreground: the brand line is faint, the
    /// compact wordmark bold, and neither is given a colour.
    #[test]
    fn words_are_bold_or_faint_and_never_coloured() {
        for paint in [Paint::Truecolor, Paint::Indexed, Paint::Basic] {
            let full_lines = render(full(Glyphs::Unicode, paint));
            let last = full_lines.last().unwrap();
            assert!(
                last.ends_with(&format!("{FAINT}{BRAND_LINE}{RESET}")),
                "{paint:?}: {last:?}"
            );
            for line in &full_lines[1..7] {
                // The mark's run closes before the wordmark starts, and nothing
                // opens after it.
                let closed = line.rfind(RESET).expect("the mark is painted") + RESET.len();
                let after = &line[closed..];
                let plain = console::strip_ansi_codes(line);
                let wordmark: String = plain.chars().skip(MARGIN + WORDMARK_COLUMN).collect();
                assert!(
                    !after.contains('\u{1b}') && after.ends_with(&wordmark),
                    "{paint:?}: the wordmark is painted: {line:?}"
                );
            }
            let compact_lines = render(compact(Glyphs::Unicode, paint));
            assert!(
                compact_lines[1].ends_with(&format!("{BOLD}{SET_WORDMARK}{RESET}")),
                "{paint:?}: {:?}",
                compact_lines[1]
            );
            assert!(
                compact_lines[2].ends_with(&format!("{FAINT}{BRAND_LINE}{RESET}")),
                "{paint:?}: {:?}",
                compact_lines[2]
            );
        }
    }

    /// Every rendering fits its width budget and the nine-row budget.
    #[test]
    fn every_line_fits_the_width_budget() {
        for style in all_styles() {
            let budget = match style.lockup {
                Lockup::Full => WIDTH,
                Lockup::Compact => COMPACT_WIDTH,
            };
            let drawn = render(style);
            assert!(drawn.len() <= ROWS, "{style:?} draws {} rows", drawn.len());
            for line in &drawn {
                let width = console::measure_text_width(line);
                assert!(
                    width <= budget,
                    "{style:?} draws {line:?} {width} columns wide, past {budget}"
                );
            }
        }
        let widest = |style: Style| {
            render(style)
                .iter()
                .map(|line| console::measure_text_width(line))
                .max()
        };
        assert_eq!(
            widest(full(Glyphs::Unicode, Paint::Truecolor)),
            Some(WIDTH),
            "WIDTH no longer names the widest line"
        );
        assert_eq!(
            widest(compact(Glyphs::Unicode, Paint::Truecolor)),
            Some(COMPACT_WIDTH),
            "COMPACT_WIDTH no longer names the widest compact line"
        );
    }

    /// Colour is escapes and nothing else.
    #[test]
    fn stripping_the_escapes_yields_the_plain_logo() {
        for style in all_styles() {
            if style.fill == Fill::Cells && style.paint != Paint::None {
                continue;
            }
            let plain = render(Style {
                paint: Paint::None,
                ..style
            });
            let stripped: Vec<String> = render(style)
                .iter()
                .map(|line| console::strip_ansi_codes(line).into_owned())
                .collect();
            assert_eq!(stripped, plain, "{style:?} changed more than the colour");
        }
    }

    /// No colour means no escape bytes at all, whatever the glyphs.
    #[test]
    fn no_colour_writes_no_escapes() {
        for style in all_styles() {
            if style.paint != Paint::None {
                continue;
            }
            for line in render(style) {
                assert!(!line.contains('\u{1b}'), "{line:?} carries an escape");
            }
        }
    }

    /// Every painted run is closed, so no colour leaks past the logo, and no
    /// line pads past its last glyph.
    #[test]
    fn every_painted_run_is_reset() {
        for style in all_styles() {
            for line in render(style) {
                if let Some(last) = line.rfind('\u{1b}') {
                    assert!(
                        line[last..].starts_with(RESET),
                        "{style:?} leaves {line:?} painting"
                    );
                }
                assert!(!line.ends_with(' '), "{line:?} pads past its last glyph");
            }
        }
    }

    /// Truecolor paints the brand's stops at the ends of each gradient, on
    /// both drawings.
    #[test]
    fn truecolor_reaches_the_brand_stops() {
        for art in [&FULL, &COMPACT] {
            let at =
                |piece: Piece, (x, y): (f32, f32)| ink_escape(art, Paint::Truecolor, piece, x, y);
            assert_eq!(
                at(Piece::Arm, art.arm[0]).as_deref(),
                Some("\u{1b}[38;2;174;90;255m")
            );
            assert_eq!(
                at(Piece::Arm, art.arm[1]).as_deref(),
                Some("\u{1b}[38;2;108;72;250m")
            );
            assert_eq!(
                at(Piece::Leg, art.leg[0]).as_deref(),
                Some("\u{1b}[38;2;91;85;253m")
            );
            assert_eq!(
                at(Piece::Leg, art.leg[1]).as_deref(),
                Some("\u{1b}[38;2;59;116;251m")
            );
            assert_eq!(at(Piece::Wordmark, (20.0, 5.0)), None);
        }
    }

    /// The gradients sit where `kin-icon.svg` puts them, scaled to each
    /// drawing: at eighteen pixels tall one pixel is 22.67 of its units.
    #[test]
    fn the_gradients_follow_the_brand_icon() {
        let close = |a: Gradient, b: Gradient| {
            a.iter()
                .zip(&b)
                .all(|(p, q)| (p.0 - q.0).abs() < 0.01 && (p.1 - q.1).abs() < 0.01)
        };
        assert!(
            close(FULL.arm, [(12.61, 0.0), (0.0, 12.61)]),
            "{:?}",
            FULL.arm
        );
        assert!(
            close(FULL.leg, [(3.18, 11.34), (14.60, 18.0)]),
            "{:?}",
            FULL.leg
        );
        assert!(
            close(COMPACT.arm, [(7.01, 0.0), (0.0, 7.01)]),
            "{:?}",
            COMPACT.arm
        );
        assert!(
            close(COMPACT.leg, [(1.76, 6.30), (8.11, 10.0)]),
            "{:?}",
            COMPACT.leg
        );
    }

    /// Each lower colour depth keeps the arm and the leg apart.
    #[test]
    fn every_colour_depth_tells_the_arm_from_the_leg() {
        for paint in [Paint::Truecolor, Paint::Indexed, Paint::Basic] {
            let arm = ink_escape(&FULL, paint, Piece::Arm, 3.0, 3.0);
            let leg = ink_escape(&FULL, paint, Piece::Leg, 7.0, 15.0);
            assert!(
                arm.is_some() && leg.is_some(),
                "{paint:?} left a piece bare"
            );
            assert_ne!(arm, leg, "{paint:?} paints the arm and the leg alike");
        }
    }

    /// No terminal cell carries a pixel of the arm over a pixel of the leg.
    ///
    /// Such a cell would have to be one glyph in one colour, and the split
    /// would close there.
    #[test]
    fn no_cell_holds_both_pieces() {
        for art in ARTS.into_iter().filter(|art| art.fill == Fill::HalfBlocks) {
            for row in 0..art.pixels.len() / 2 {
                let top = art.pixels[2 * row].as_bytes();
                let bottom = art.pixels[2 * row + 1].as_bytes();
                for column in 0..top.len() {
                    if let (Some(upper), Some(lower)) =
                        (pixel_piece(top[column]), pixel_piece(bottom[column]))
                    {
                        assert_eq!(upper, lower, "row {row}, column {column} mixes pieces");
                    }
                }
            }
        }
    }

    /// The split is a channel: no arm pixel touches a leg pixel, even at a
    /// corner.
    ///
    /// Corner contact is what a one-pixel diagonal gap degrades to when one
    /// pixel moves, and at terminal size it reads as the two bands joining.
    #[test]
    fn the_arm_and_the_leg_never_touch() {
        for art in ARTS {
            let at = |row: isize, column: isize| -> Option<Piece> {
                let row = usize::try_from(row).ok()?;
                let column = usize::try_from(column).ok()?;
                art.pixels
                    .get(row)
                    .and_then(|line| line.as_bytes().get(column))
                    .and_then(|&pixel| pixel_piece(pixel))
            };
            for (row, line) in art.pixels.iter().enumerate() {
                for (column, &pixel) in line.as_bytes().iter().enumerate() {
                    if pixel_piece(pixel) != Some(Piece::Arm) {
                        continue;
                    }
                    for dy in -1..=1 {
                        for dx in -1..=1 {
                            assert_ne!(
                                at(row as isize + dy, column as isize + dx),
                                Some(Piece::Leg),
                                "the arm at row {row}, column {column} touches the leg"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_pixel_row_is_the_same_width() {
        for art in ARTS {
            for line in art.pixels {
                assert_eq!(line.len(), art.pixels[0].len(), "{line:?}");
            }
            if art.fill == Fill::HalfBlocks {
                assert_eq!(
                    art.pixels.len() % 2,
                    0,
                    "a pixel row is left without a partner"
                );
                assert_eq!(art.ascii.len(), art.rows());
            }
        }
        assert_eq!(
            CELLS[0].len(),
            PIXELS[0].len(),
            "the two full drawings differ in width"
        );
    }

    /// An ordinary terminal gets the full logo.
    #[test]
    fn a_truecolor_utf8_terminal_gets_the_gradient_blocks() {
        assert_eq!(decide(&[]), Some(full(Glyphs::Unicode, Paint::Truecolor)));
    }

    /// A pipe or a file is being read by something, and gets nothing.
    #[test]
    fn a_pipe_gets_nothing() {
        assert_eq!(decide_with(false, None, false, false, &[]), None);
    }

    /// `--json`, and any other output a program reads, gets nothing.
    #[test]
    fn machine_output_gets_nothing() {
        let read = |_: &str| -> Option<String> { None };
        let surroundings = Surroundings {
            stdout_is_terminal: true,
            columns: Some(120),
            windows: false,
            utf8_console: false,
            var: &read,
        };
        assert_eq!(plan(&surroundings, false), None);
        assert!(!print_once(false), "a machine-mode caller printed the logo");
    }

    #[test]
    fn ci_gets_nothing() {
        for value in ["true", "1", "yes", "github"] {
            assert_eq!(decide(&[("CI", value)]), None, "CI={value}");
        }
        for value in ["false", "0", "no", "off", ""] {
            assert!(decide(&[("CI", value)]).is_some(), "CI={value:?}");
        }
    }

    #[test]
    fn kin_no_banner_opts_out() {
        for value in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert_eq!(
                decide(&[("KIN_NO_BANNER", value)]),
                None,
                "KIN_NO_BANNER={value:?}"
            );
        }
        for value in ["0", "false", "no", "off", ""] {
            assert!(
                decide(&[("KIN_NO_BANNER", value)]).is_some(),
                "KIN_NO_BANNER={value:?}"
            );
        }
    }

    /// `NO_COLOR` is about colour, so the logo stays and only the escapes go.
    #[test]
    fn no_color_keeps_the_logo_without_colour() {
        for value in ["1", ""] {
            assert_eq!(
                decide(&[("NO_COLOR", value)]),
                Some(full(Glyphs::Unicode, Paint::None)),
                "NO_COLOR={value:?}"
            );
        }
        assert_eq!(
            decide(&[("TERM", "dumb"), ("-COLORTERM", "")]),
            Some(full(Glyphs::Unicode, Paint::None))
        );
    }

    #[test]
    fn colour_depth_follows_the_terminal() {
        let paint = |vars: &[(&str, &str)]| decide(vars).map(|style| style.paint);
        assert_eq!(paint(&[]), Some(Paint::Truecolor));
        assert_eq!(
            paint(&[("-COLORTERM", ""), ("TERM", "xterm-256color")]),
            Some(Paint::Indexed)
        );
        assert_eq!(
            paint(&[("-COLORTERM", ""), ("TERM", "xterm")]),
            Some(Paint::Basic)
        );
        assert_eq!(
            paint(&[("-COLORTERM", ""), ("-TERM", ""), ("WT_SESSION", "1")]),
            Some(Paint::Truecolor)
        );
    }

    /// A wide terminal gets the full lockup, a narrow one the compact
    /// lockup, and one too narrow for that gets none. An unknown width is
    /// given the benefit of the doubt.
    #[test]
    fn the_width_picks_the_lockup() {
        let lockup = |columns: Option<usize>| {
            decide_with(true, columns, false, false, &[]).map(|style| style.lockup)
        };
        assert_eq!(lockup(Some(100)), Some(Lockup::Full));
        assert_eq!(lockup(Some(FULL_MIN_COLUMNS)), Some(Lockup::Full));
        assert_eq!(lockup(Some(FULL_MIN_COLUMNS - 1)), Some(Lockup::Compact));
        assert_eq!(lockup(Some(50)), Some(Lockup::Compact));
        assert_eq!(lockup(Some(COMPACT_WIDTH)), Some(Lockup::Compact));
        assert_eq!(lockup(Some(COMPACT_WIDTH - 1)), None);
        assert_eq!(lockup(None), Some(Lockup::Full));
        // The literals, so a constant that drifts is caught rather than
        // followed: the compact lockup is 39 columns, the full one 54, and
        // the full one is chosen from 60.
        assert_eq!(COMPACT_WIDTH, 39);
        assert_eq!(WIDTH, 54);
        assert_eq!(FULL_MIN_COLUMNS, 60);
    }

    #[test]
    fn a_utf8_locale_gets_blocks_and_any_other_gets_ascii() {
        let glyphs = |vars: &[(&str, &str)]| decide(vars).map(|style| style.glyphs);
        assert_eq!(glyphs(&[("LANG", "en_US.UTF-8")]), Some(Glyphs::Unicode));
        assert_eq!(glyphs(&[("LANG", "C")]), Some(Glyphs::Ascii));
        assert_eq!(
            glyphs(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")]),
            Some(Glyphs::Ascii),
            "LC_ALL outranks LANG"
        );
        assert_eq!(
            glyphs(&[("LC_CTYPE", "C.UTF-8"), ("LANG", "C")]),
            Some(Glyphs::Unicode),
            "LC_CTYPE outranks LANG"
        );
        assert_eq!(
            glyphs(&[("-LANG", "")]),
            Some(Glyphs::Ascii),
            "a Unix process with no locale is in the C locale"
        );
    }

    /// Windows sets no locale, so the console itself decides.
    #[test]
    fn windows_draws_blocks_only_where_the_console_reads_utf8() {
        let glyphs = |utf8_console: bool, vars: &[(&str, &str)]| {
            decide_with(true, Some(120), true, utf8_console, vars).map(|style| style.glyphs)
        };
        assert_eq!(
            glyphs(false, &[("-LANG", ""), ("WT_SESSION", "{guid}")]),
            Some(Glyphs::Unicode),
            "Windows Terminal"
        );
        assert_eq!(
            glyphs(true, &[("-LANG", "")]),
            Some(Glyphs::Unicode),
            "a console on code page 65001"
        );
        assert_eq!(
            glyphs(false, &[("-LANG", "")]),
            Some(Glyphs::Ascii),
            "a console on a legacy code page"
        );
        assert_eq!(
            glyphs(false, &[("LANG", "en_US.UTF-8")]),
            Some(Glyphs::Unicode),
            "a shell that sets a UTF-8 locale, such as Git Bash"
        );
    }

    /// A terminal too short for the full lockup and the rows its caller needs
    /// under it gets the compact one: 80x24 cannot hold nine rows of art and
    /// setup's first question together.
    #[test]
    fn a_short_terminal_leaves_room_for_what_follows() {
        assert!(full_lockup_fits(Some(40), Some(20)));
        assert!(!full_lockup_fits(Some(24), Some(16)));
        assert!(
            full_lockup_fits(None, Some(16)),
            "an unknown height keeps the full lockup"
        );
        assert!(
            full_lockup_fits(Some(24), None),
            "a caller that needs nothing keeps it"
        );
    }

    /// The first moment takes the slot and every later one finds it taken.
    #[test]
    fn the_logo_prints_once_per_process() {
        let flag = AtomicBool::new(false);
        assert!(claim(&flag));
        assert!(!claim(&flag));
        assert!(!claim(&flag));
    }

    /// The frame is one blank line above and one below.
    #[test]
    fn the_logo_is_framed_by_blank_lines() {
        let block = framed(&render(full(Glyphs::Unicode, Paint::None)));
        assert!(block.starts_with("\n     ▄"), "{block:?}");
        assert!(block.ends_with("code.\n\n"), "{block:?}");
        assert_eq!(block.lines().count(), ROWS + 2);
    }

    /// The terminals a first run meets, by what they set: the colour depth
    /// each one gets, and the escapes that depth may and may not write.
    const COLOUR_MODES: [(&str, &[(&str, &str)], Paint); 6] = [
        ("truecolor", &[], Paint::Truecolor),
        (
            "256 colours",
            &[("-COLORTERM", ""), ("TERM", "xterm-256color")],
            Paint::Indexed,
        ),
        (
            "16 colours",
            &[("-COLORTERM", ""), ("TERM", "xterm")],
            Paint::Basic,
        ),
        ("NO_COLOR", &[("NO_COLOR", "1")], Paint::None),
        (
            "TERM=dumb",
            &[("-COLORTERM", ""), ("TERM", "dumb")],
            Paint::None,
        ),
        (
            "NO_COLOR on a truecolor terminal",
            &[("NO_COLOR", ""), ("COLORTERM", "truecolor")],
            Paint::None,
        ),
    ];

    /// Each colour mode writes only its own kind of escape, at every width
    /// that draws a logo, and every line fits the terminal it was drawn for.
    #[test]
    fn each_colour_mode_writes_only_its_own_escapes_at_each_width() {
        for (name, vars, paint) in COLOUR_MODES {
            for columns in [200, 120, 80, FULL_MIN_COLUMNS, 59, 50, COMPACT_WIDTH] {
                let style = decide_with(true, Some(columns), false, false, vars)
                    .unwrap_or_else(|| panic!("{name} at {columns} columns drew nothing"));
                assert_eq!(style.paint, paint, "{name} at {columns} columns");
                let expected = if columns >= FULL_MIN_COLUMNS {
                    Lockup::Full
                } else {
                    Lockup::Compact
                };
                assert_eq!(style.lockup, expected, "{name} at {columns} columns");
                let drawn = render(style).join("\n");
                let has = |escape: &str| drawn.contains(escape);
                match paint {
                    Paint::Truecolor => {
                        assert!(has("\u{1b}[38;2;"), "{name}: no 24-bit escape");
                        assert!(!has("\u{1b}[38;5;"), "{name}: a 256-colour escape");
                    }
                    Paint::Indexed => {
                        assert!(has("\u{1b}[38;5;135m"), "{name}: the arm is not 135");
                        assert!(has("\u{1b}[38;5;69m"), "{name}: the leg is not 69");
                        assert!(!has("\u{1b}[38;2;"), "{name}: a 24-bit escape");
                    }
                    Paint::Basic => {
                        assert!(has(crate::mark::BASIC_ARM), "{name}: no magenta arm");
                        assert!(has(crate::mark::BASIC_LEG), "{name}: no blue leg");
                        assert!(!has("\u{1b}[38;"), "{name}: an extended colour escape");
                    }
                    Paint::None => assert!(!has("\u{1b}"), "{name}: an escape"),
                }
                for line in render(style) {
                    let width = console::measure_text_width(&line);
                    assert!(
                        width <= columns,
                        "{name} at {columns} columns draws {line:?} {width} wide"
                    );
                }
            }
            assert_eq!(
                decide_with(true, Some(COMPACT_WIDTH - 1), false, false, vars),
                None,
                "{name} drew a logo too narrow to hold one"
            );
        }
    }

    /// Terminal.app gets painted cells, which fill the whole cell, instead
    /// of half blocks it would draw as tiles. Only colour can paint a cell,
    /// so without it Terminal.app keeps the plain half blocks, and no other
    /// terminal changes.
    #[test]
    fn terminal_app_gets_painted_cells_while_it_has_colour() {
        let apple = ("TERM_PROGRAM", "Apple_Terminal");
        let fill = |vars: &[(&str, &str)]| decide(vars).map(|style| style.fill);
        assert_eq!(
            fill(&[apple, ("-COLORTERM", "")]),
            Some(Fill::Cells),
            "Terminal.app on 256 colours"
        );
        assert_eq!(
            fill(&[apple]),
            Some(Fill::Cells),
            "Terminal.app on truecolor"
        );
        assert_eq!(fill(&[apple, ("NO_COLOR", "1")]), Some(Fill::HalfBlocks));
        assert_eq!(fill(&[]), Some(Fill::HalfBlocks));
        for other in ["iTerm.app", "vscode", "ghostty", "WezTerm", "tmux", ""] {
            assert_eq!(
                fill(&[("TERM_PROGRAM", other)]),
                Some(Fill::HalfBlocks),
                "TERM_PROGRAM={other:?}"
            );
        }
        let no_colour = decide(&[apple, ("NO_COLOR", "1")]).unwrap();
        for line in render(no_colour) {
            assert!(!line.contains('\u{1b}'), "{line:?}");
        }
    }

    /// What a rendered cell row paints, read back as the drawing's letters:
    /// `A` and `L` for the two 256-colour backgrounds, `W` for reverse video,
    /// and any other character as itself.
    fn painted(line: &str) -> String {
        let mut out = String::new();
        let mut state = ' ';
        let mut rest = line;
        while let Some(start) = rest.find('\u{1b}') {
            for glyph in rest[..start].chars() {
                out.push(if glyph == ' ' { state } else { glyph });
            }
            let end = rest[start..].find('m').expect("an SGR escape") + start;
            state = match &rest[start..=end] {
                "\u{1b}[48;5;135m" => 'A',
                "\u{1b}[48;5;69m" => 'L',
                "\u{1b}[7m" => 'W',
                _ => ' ',
            };
            rest = &rest[end + 1..];
        }
        for glyph in rest.chars() {
            out.push(if glyph == ' ' { state } else { glyph });
        }
        out
    }

    /// The cell drawings print exactly the cells they name, as painted spaces
    /// with no glyph, the wordmark in reverse video and the words in place.
    #[test]
    fn a_cell_drawing_paints_exactly_its_cells() {
        let full = render(in_cells(Lockup::Full, Paint::Indexed));
        assert_eq!(full.len(), ROWS);
        for (row, line) in full.iter().enumerate() {
            let expected = format!("  {}", CELLS[row].replace('.', " "));
            let expected = expected.trim_end();
            let read = painted_line(line);
            if row == BRAND_LINE_ROW {
                assert!(read.starts_with(expected), "row {row}: {read:?}");
                assert!(read.ends_with(BRAND_LINE), "row {row}: {read:?}");
            } else {
                assert_eq!(read, expected, "row {row}");
            }
            assert!(
                !line.contains(['\u{2580}', '\u{2584}', '\u{2588}']),
                "a cell drawing draws no block glyph: {line:?}"
            );
        }
        let compact = render(in_cells(Lockup::Compact, Paint::Indexed));
        assert_eq!(compact.len(), COMPACT_CELLS.len());
        assert!(painted_line(&compact[COMPACT_CELLS_WORDMARK_ROW]).ends_with(SET_WORDMARK));
        assert!(painted_line(&compact[COMPACT_CELLS_WORDMARK_ROW + 1]).ends_with(BRAND_LINE));
        assert_eq!(painted_line(&compact[0]), "    AAAAA");
        assert_eq!(painted_line(&compact[5]), "     LLLLLL");
    }

    /// [`painted`] over a whole rendered line.
    fn painted_line(line: &str) -> String {
        painted(line).trim_end().to_string()
    }

    /// Each colour depth paints cells with its own kind of background, and
    /// every run is closed before the next opens, so reverse video never
    /// leaks into a colour.
    #[test]
    fn cells_paint_backgrounds_in_each_colour_depth() {
        for lockup in [Lockup::Full, Lockup::Compact] {
            let truecolor = render(in_cells(lockup, Paint::Truecolor)).join("\n");
            assert!(truecolor.contains("\u{1b}[48;2;"), "{lockup:?}");
            assert!(
                !truecolor.contains("\u{1b}[38;"),
                "{lockup:?}: a foreground colour"
            );
            let basic = render(in_cells(lockup, Paint::Basic)).join("\n");
            assert!(basic.contains(BASIC_ARM_BACKGROUND) && basic.contains(BASIC_LEG_BACKGROUND));
            for paint in [Paint::Truecolor, Paint::Indexed, Paint::Basic] {
                for line in render(in_cells(lockup, paint)) {
                    let mut open = false;
                    let mut rest = line.as_str();
                    while let Some(start) = rest.find('\u{1b}') {
                        let end = rest[start..].find('m').unwrap() + start;
                        let escape = &rest[start..=end];
                        if escape == RESET {
                            open = false;
                        } else if escape.starts_with("\u{1b}[48") || escape == REVERSE {
                            assert!(
                                !open,
                                "{lockup:?} {paint:?} opens a run inside another: {line:?}"
                            );
                            open = true;
                        } else {
                            open = false;
                        }
                        rest = &rest[end + 1..];
                    }
                    assert!(
                        line.ends_with(RESET) || !line.contains('\u{1b}'),
                        "{line:?}"
                    );
                }
            }
        }
    }

    /// Bold and faint are not colour: they stay when colour is on and go
    /// with it, so a `NO_COLOR` reader gets plain words.
    #[test]
    fn words_carry_weight_only_when_colour_is_on() {
        for (name, vars, paint) in COLOUR_MODES {
            for columns in [120, 50] {
                let style = decide_with(true, Some(columns), false, false, vars).unwrap();
                let drawn = render(style).join("\n");
                let weighted = drawn.contains(FAINT);
                assert_eq!(weighted, paint != Paint::None, "{name} at {columns}");
            }
        }
    }

    /// The ASCII drawings are ASCII, so a C locale never sees a stray byte.
    #[test]
    fn the_ascii_drawings_are_ascii() {
        for lockup in [Lockup::Full, Lockup::Compact] {
            for paint in [Paint::Truecolor, Paint::Indexed, Paint::Basic, Paint::None] {
                for fill in [Fill::HalfBlocks, Fill::Cells] {
                    for line in render(Style {
                        lockup,
                        glyphs: Glyphs::Ascii,
                        paint,
                        fill,
                    }) {
                        let plain = console::strip_ansi_codes(&line);
                        assert!(plain.is_ascii(), "{lockup:?} {paint:?} {fill:?}: {plain:?}");
                    }
                }
            }
        }
    }

    /// The compact mark is the same mark: its two bands run the same ways
    /// and its split is in the same place, the arm's point above the leg's
    /// cut.
    #[test]
    fn the_compact_mark_keeps_the_marks_bands_and_split() {
        for art in ARTS {
            let rows_of = |piece: u8| -> Vec<usize> {
                art.pixels
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.as_bytes().contains(&piece))
                    .map(|(row, _)| row)
                    .collect()
            };
            let arm = rows_of(b'A');
            let leg = rows_of(b'L');
            assert_eq!(arm.first(), Some(&0), "the arm starts the mark");
            assert_eq!(leg.last(), Some(&(art.pixels.len() - 1)), "the leg ends it");
            assert!(arm.last() < leg.last() && arm.first() < leg.first());
            // The arm runs down to the left and the leg down to the right.
            let first = |row: usize, piece: u8| art.pixels[row].bytes().position(|p| p == piece);
            let last = |row: usize, piece: u8| art.pixels[row].bytes().rposition(|p| p == piece);
            assert!(last(arm[0], b'A') > last(arm[arm.len() - 1], b'A'));
            assert!(last(leg[0], b'L') < last(leg[leg.len() - 1], b'L'));
            assert!(first(arm[0], b'A') > first(arm[arm.len() - 1], b'A'));
        }
    }
}
