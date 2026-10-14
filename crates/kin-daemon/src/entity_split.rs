// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! One content's changes, separated by the entity each one belongs to.
//!
//! Keeping some of the working copy's entities and leaving the rest
//! uncommitted needs, for every content a chosen entity lives in, the text
//! that carries exactly the chosen entities' changes and none of the others.
//! Undoing one entity needs the same thing in the other direction. This module
//! takes two versions of one content, the entities each version holds with
//! their exact spans, and the chosen set, and builds that text. It reads
//! nothing and writes nothing: the caller derives the result's entities again
//! and proves them before anything is published.
//!
//! A change is attributed by where it lands. The two versions are compared
//! line by line, and every changed region belongs to the entities whose own
//! text it touches, an entity's own text being its span less the spans of the
//! entities inside it. A region that belongs only to chosen entities is
//! carried, one that belongs to none of them is left, and one that belongs to
//! both is separated at entity boundaries when its lines fall cleanly on one
//! side of them, and refused when they do not. Text no entity holds, such as
//! an import line between declarations, is carried only when the caller's
//! [`Outside`] rule says so.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use kin_model::{EntityId, EntityKind};

/// One entity as one version of a content holds it.
#[derive(Debug, Clone)]
pub(crate) struct Spanned {
    pub(crate) id: EntityId,
    pub(crate) name: String,
    pub(crate) kind: EntityKind,
    pub(crate) span: Range<usize>,
}

impl Spanned {
    /// An entity that stands for its whole content, whose own text is the
    /// text between every other entity in it. Changes there belong to no
    /// entity a reader reviews one at a time.
    fn is_whole_unit(&self) -> bool {
        matches!(
            self.kind,
            EntityKind::Module | EntityKind::File | EntityKind::Package
        )
    }
}

/// One version of one content: its bytes and the entities it holds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Version<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) entities: &'a [Spanned],
}

/// What happens to changed text that no entity holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outside {
    /// Carried when every entity the content's changes touch is chosen, so a
    /// content whose every changed entity is chosen is carried whole.
    WhenEveryEntityIsChosen,
    /// Never carried.
    Never,
}

/// The content that carries the chosen entities' changes.
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) bytes: Vec<u8>,
    /// Every entity whose own text the changes between the two versions touch,
    /// chosen or not.
    pub(crate) changed: BTreeSet<EntityId>,
}

/// A chosen entity whose change shares lines with an entity that was not
/// chosen, so neither can be carried without the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entangled {
    pub(crate) chosen: String,
    pub(crate) other: String,
}

/// Build the content `from` becomes when exactly the chosen entities take the
/// text they have in `to`.
pub(crate) fn select(
    from: Version<'_>,
    to: Version<'_>,
    chosen: &HashSet<EntityId>,
    outside: Outside,
) -> Result<Selected, Entangled> {
    let mut from = Side::new(from);
    let mut to = Side::new(to);
    let mut hunks = Vec::new();
    let (from_lines, to_lines) = (from.lines.len(), to.lines.len());
    align(
        &mut from,
        &mut to,
        0..from_lines,
        0..to_lines,
        None,
        &mut hunks,
    );

    let mut changed = BTreeSet::new();
    for hunk in &hunks {
        changed.extend(from.own_in(hunk.from.clone()));
        changed.extend(to.own_in(hunk.to.clone()));
    }
    let carry_outside =
        outside == Outside::WhenEveryEntityIsChosen && changed.iter().all(|id| chosen.contains(id));
    let names = from.names().chain(to.names()).collect::<HashMap<_, _>>();
    let decide = Decide {
        chosen,
        carry_outside,
        names: &names,
    };

    let mut bytes = Vec::with_capacity(to.version.bytes.len().max(from.version.bytes.len()));
    let mut cursor = 0;
    for hunk in &hunks {
        from.push_lines(&mut bytes, cursor..hunk.from.start);
        decide.hunk(hunk, &mut from, &mut to, &mut bytes)?;
        cursor = hunk.from.end;
    }
    from.push_lines(&mut bytes, cursor..from.lines.len());
    Ok(Selected { bytes, changed })
}

/// One changed region, in lines of each version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    from: Range<usize>,
    to: Range<usize>,
}

/// One line's reading: which entities' own text it holds, and which entity it
/// sits in when read from outside.
#[derive(Debug, Clone, Default)]
struct Reading {
    /// The outermost entity holding any of the line's non-blank text.
    owner: Option<EntityId>,
    /// The innermost entity of each non-blank byte.
    own: BTreeSet<EntityId>,
    /// The innermost entity of the line's first non-blank byte: the entity
    /// the line is a line of.
    of: Option<EntityId>,
    blank: bool,
}

struct Side<'a> {
    version: Version<'a>,
    lines: Vec<Range<usize>>,
    readings: Vec<Option<Reading>>,
}

impl<'a> Side<'a> {
    fn new(version: Version<'a>) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in version.bytes.iter().enumerate() {
            if *byte == b'\n' {
                lines.push(start..index + 1);
                start = index + 1;
            }
        }
        if start < version.bytes.len() {
            lines.push(start..version.bytes.len());
        }
        let readings = vec![None; lines.len()];
        Self {
            version,
            lines,
            readings,
        }
    }

    fn text(&self, line: usize) -> &'a [u8] {
        &self.version.bytes[self.lines[line].clone()]
    }

    fn names(&self) -> impl Iterator<Item = (EntityId, String)> + '_ {
        self.version
            .entities
            .iter()
            .map(|entity| (entity.id, entity.name.clone()))
    }

    fn reading(&mut self, line: usize) -> &Reading {
        if self.readings[line].is_none() {
            let reading = self.read(line);
            self.readings[line] = Some(reading);
        }
        self.readings[line].as_ref().expect("read above")
    }

    fn read(&self, line: usize) -> Reading {
        let range = self.lines[line].clone();
        let mut candidates = self
            .version
            .entities
            .iter()
            .filter(|entity| {
                !entity.is_whole_unit()
                    && entity.span.start < range.end
                    && range.start < entity.span.end
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|entity| entity.span.len());
        let mut reading = Reading {
            blank: true,
            ..Reading::default()
        };
        let mut outermost: Option<&Spanned> = None;
        for at in range {
            if self.version.bytes[at].is_ascii_whitespace() {
                continue;
            }
            let first = reading.blank;
            reading.blank = false;
            let mut holding = candidates.iter().filter(|entity| entity.span.contains(&at));
            if let Some(innermost) = holding.next() {
                reading.own.insert(innermost.id);
                if first {
                    reading.of = Some(innermost.id);
                }
            }
            if let Some(widest) = candidates
                .iter()
                .rev()
                .find(|entity| entity.span.contains(&at))
            {
                if outermost.is_none_or(|held| widest.span.len() > held.span.len()) {
                    outermost = Some(widest);
                }
            }
        }
        reading.owner = outermost.map(|entity| entity.id);
        reading
    }

    fn own_in(&mut self, lines: Range<usize>) -> BTreeSet<EntityId> {
        let mut own = BTreeSet::new();
        for line in lines {
            own.extend(self.reading(line).own.iter().copied());
        }
        own
    }

    fn push_lines(&self, out: &mut Vec<u8>, lines: Range<usize>) {
        if lines.is_empty() {
            return;
        }
        let start = self.lines[lines.start].start;
        let end = self.lines[lines.end - 1].end;
        out.extend_from_slice(&self.version.bytes[start..end]);
    }

    /// The lines of `range` gathered into runs that each belong to one
    /// outermost entity, or to none.
    ///
    /// Unowned lines directly above an entity with no blank line between
    /// travel with the entity below them: they are its leading comments.
    /// Blank lines are spacing, and travel with the entity they separate in
    /// the direction the run is written: a run that opens on an entity and
    /// closes on spacing carries each entity's spacing after it, any other run
    /// before it. Unowned text separated from the next entity by a blank line,
    /// or after the last one, is a group of its own.
    fn groups(&mut self, range: Range<usize>) -> Vec<Group> {
        let spacing_follows = !range.is_empty()
            && self.reading(range.start).owner.is_some()
            && self.reading(range.end - 1).blank;
        let mut groups: Vec<Group> = Vec::new();
        let mut pending: Vec<usize> = Vec::new();
        for line in range.clone() {
            let reading = self.reading(line).clone();
            match reading.owner {
                Some(owner) => {
                    if spacing_follows {
                        if let Some(last) = groups.last_mut() {
                            if last.owner.is_some() && pending.first() == Some(&last.lines.end) {
                                let spacing = pending
                                    .iter()
                                    .take_while(|line| {
                                        self.readings[**line].as_ref().is_some_and(|r| r.blank)
                                    })
                                    .count();
                                last.lines.end += spacing;
                                pending.drain(..spacing);
                            }
                        }
                    }
                    let split = pending
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(position, line)| {
                            !self.reading_cached(**line).blank
                                && pending
                                    .get(position + 1)
                                    .is_some_and(|next| self.reading_cached(*next).blank)
                        })
                        .map_or(0, |(position, _)| position + 1);
                    if split > 0 {
                        groups.push(Group {
                            lines: pending[0]..pending[split - 1] + 1,
                            owner: None,
                            own: BTreeSet::new(),
                        });
                    }
                    let start = pending.get(split).copied().unwrap_or(line);
                    pending.clear();
                    match groups.last_mut() {
                        Some(last) if last.owner == Some(owner) && last.lines.end == start => {
                            last.lines.end = line + 1;
                            last.own.extend(reading.own.iter().copied());
                        }
                        _ => groups.push(Group {
                            lines: start..line + 1,
                            owner: Some(owner),
                            own: reading.own.clone(),
                        }),
                    }
                }
                None => pending.push(line),
            }
        }
        if !pending.is_empty() {
            // Blank lines straight after an entity are its trailing spacing.
            let trailing = pending
                .iter()
                .take_while(|line| self.reading_cached(**line).blank)
                .count();
            let attach = match groups.last_mut() {
                Some(last) if last.lines.end == pending[0] => {
                    last.lines.end += trailing;
                    trailing
                }
                _ => 0,
            };
            if attach < pending.len() {
                groups.push(Group {
                    lines: pending[attach]..range.end,
                    owner: None,
                    own: BTreeSet::new(),
                });
            }
        }
        groups
    }

    /// Whether two lines of this version read the same and are lines of the
    /// same entity.
    fn same_line(&mut self, left: usize, right: usize) -> bool {
        let of = self.reading(left).of;
        self.text(left) == self.text(right) && of == self.reading(right).of
    }

    /// The blank lines directly above `line`, as bytes.
    fn spacing_above(&mut self, line: usize) -> Vec<u8> {
        let mut first = line;
        while first > 0 && self.reading(first - 1).blank {
            first -= 1;
        }
        let mut spacing = Vec::new();
        self.push_lines(&mut spacing, first..line);
        spacing
    }

    fn reading_cached(&self, line: usize) -> &Reading {
        self.readings[line]
            .as_ref()
            .expect("every grouped line is read before it is grouped")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    From,
    To,
}

/// One run of lines a separated region is rebuilt from.
#[derive(Debug, Clone)]
struct Piece {
    origin: Origin,
    lines: Range<usize>,
    /// Whether it is an entity only its own version holds.
    alone: bool,
}

/// A run of lines that belongs to one outermost entity, or to none.
#[derive(Debug, Clone)]
struct Group {
    lines: Range<usize>,
    owner: Option<EntityId>,
    own: BTreeSet<EntityId>,
}

/// One entity's whole lines in one version: the line its span starts on
/// through the line it ends on.
#[derive(Debug, Clone)]
struct Block {
    id: EntityId,
    lines: Range<usize>,
}

impl Side<'_> {
    /// The line holding `byte`.
    fn line_of(&self, byte: usize) -> usize {
        self.lines
            .partition_point(|line| line.end <= byte)
            .min(self.lines.len().saturating_sub(1))
    }

    /// The outermost entities whose whole lines lie inside `range`, in order,
    /// leaving out `parent` itself. An entity that shares a line with a
    /// sibling has no lines of its own to anchor on and is left out too.
    fn blocks(&self, range: Range<usize>, parent: Option<EntityId>) -> Vec<Block> {
        let mut candidates = self
            .version
            .entities
            .iter()
            .filter(|entity| {
                !entity.is_whole_unit() && !entity.span.is_empty() && Some(entity.id) != parent
            })
            .filter(|entity| entity.span.end <= self.version.bytes.len())
            .map(|entity| {
                let lines = self.line_of(entity.span.start)..self.line_of(entity.span.end - 1) + 1;
                (entity, lines)
            })
            .filter(|(_, lines)| range.start <= lines.start && lines.end <= range.end)
            .collect::<Vec<_>>();
        candidates.sort_by(|(left, left_lines), (right, right_lines)| {
            left_lines
                .start
                .cmp(&right_lines.start)
                .then(right_lines.end.cmp(&left_lines.end))
                .then(right.span.len().cmp(&left.span.len()))
        });
        let mut tops: Vec<(&Spanned, Range<usize>, bool)> = Vec::new();
        for (entity, lines) in candidates {
            match tops.last_mut() {
                Some((top, top_lines, shared)) if lines.start < top_lines.end => {
                    let nested =
                        top.span.start <= entity.span.start && entity.span.end <= top.span.end;
                    if !nested {
                        *shared = true;
                        top_lines.end = top_lines.end.max(lines.end);
                    }
                }
                _ => tops.push((entity, lines, false)),
            }
        }
        tops.into_iter()
            .filter(|(_, _, shared)| !shared)
            .map(|(entity, lines, _)| Block {
                id: entity.id,
                lines,
            })
            .collect()
    }
}

/// The changed regions between two versions, anchored on the entities both
/// versions hold.
///
/// Comparing the two contents as plain lines lets an inserted entity's lines
/// match a neighbour's identical lines: a new function whose last two lines
/// read like the function above it is read as an edit of that function plus a
/// stray insertion. So the entities present in both versions are aligned
/// first, by identity and in order, each aligned pair is compared inside
/// itself the same way, and only the text between aligned entities is
/// compared as lines.
fn align(
    from: &mut Side<'_>,
    to: &mut Side<'_>,
    from_range: Range<usize>,
    to_range: Range<usize>,
    parent: Option<EntityId>,
    out: &mut Vec<Hunk>,
) {
    let from_blocks = from.blocks(from_range.clone(), parent);
    let to_blocks = to.blocks(to_range.clone(), parent);
    let from_ids = from_blocks.iter().map(|block| block.id).collect::<Vec<_>>();
    let to_ids = to_blocks.iter().map(|block| block.id).collect::<Vec<_>>();
    let mut anchors = Vec::new();
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &from_ids, &to_ids) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            anchors.extend(old.zip(new));
        }
    }
    let (mut from_at, mut to_at) = (from_range.start, to_range.start);
    for (old, new) in anchors {
        let (old, new) = (&from_blocks[old], &to_blocks[new]);
        compare_lines(
            from,
            to,
            from_at..old.lines.start,
            to_at..new.lines.start,
            out,
        );
        let same = old.lines.len() == new.lines.len()
            && old
                .lines
                .clone()
                .zip(new.lines.clone())
                .all(|(left, right)| from.text(left) == to.text(right));
        if !same {
            align(
                from,
                to,
                old.lines.clone(),
                new.lines.clone(),
                Some(old.id),
                out,
            );
        }
        from_at = old.lines.end;
        to_at = new.lines.end;
    }
    compare_lines(from, to, from_at..from_range.end, to_at..to_range.end, out);
}

/// The changed regions of one stretch of text no aligned entity holds,
/// compared line by line.
fn compare_lines(
    from: &mut Side<'_>,
    to: &mut Side<'_>,
    from_range: Range<usize>,
    to_range: Range<usize>,
    out: &mut Vec<Hunk>,
) {
    if from_range.is_empty() && to_range.is_empty() {
        return;
    }
    // A line matches a line of the same entity, or of no entity, and never a
    // line of a different one: the closing brace of one function is not the
    // closing brace of the function written in its place.
    let from_text = from_range
        .clone()
        .map(|line| (from.reading(line).of, from.text(line)))
        .collect::<Vec<_>>();
    let to_text = to_range
        .clone()
        .map(|line| (to.reading(line).of, to.text(line)))
        .collect::<Vec<_>>();
    let mut hunks: Vec<Hunk> = Vec::new();
    for op in similar::capture_diff_slices(similar::Algorithm::Histogram, &from_text, &to_text) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            continue;
        }
        let old = old.start + from_range.start..old.end + from_range.start;
        let new = new.start + to_range.start..new.end + to_range.start;
        match hunks.last_mut() {
            Some(last) if last.from.end == old.start && last.to.end == new.start => {
                last.from.end = old.end;
                last.to.end = new.end;
            }
            _ => hunks.push(Hunk { from: old, to: new }),
        }
    }
    settle(&mut hunks, from, to, &from_range, &to_range);
    out.extend(hunks);
}

/// Slide every pure insertion and pure deletion to the place where it touches
/// the fewest entities.
///
/// A block of lines inserted between two equal lines can often be read at more
/// than one position: adding a function after another whose last line is the
/// same `}` matches either brace. The comparison picks one; the entity
/// boundaries say which reading is the change. Only positions the comparison
/// itself would accept are tried, so the content produced is the same either
/// way and only the attribution moves.
fn settle(
    hunks: &mut [Hunk],
    from: &mut Side<'_>,
    to: &mut Side<'_>,
    from_range: &Range<usize>,
    to_range: &Range<usize>,
) {
    for index in 0..hunks.len() {
        let (lower_from, lower_to) = match index {
            0 => (from_range.start, to_range.start),
            _ => (hunks[index - 1].from.end, hunks[index - 1].to.end),
        };
        let (upper_from, upper_to) = match hunks.get(index + 1) {
            Some(next) => (next.from.start, next.to.start),
            None => (from_range.end, to_range.end),
        };
        let hunk = hunks[index].clone();
        let settled = if hunk.from.is_empty() && !hunk.to.is_empty() {
            slide(&hunk.to, lower_to, upper_to, to).map(|(lines, shift)| Hunk {
                from: shifted(&hunk.from, shift),
                to: lines,
            })
        } else if hunk.to.is_empty() && !hunk.from.is_empty() {
            slide(&hunk.from, lower_from, upper_from, from).map(|(lines, shift)| Hunk {
                from: lines,
                to: shifted(&hunk.to, shift),
            })
        } else {
            None
        };
        if let Some(settled) = settled {
            hunks[index] = settled;
        }
    }
}

fn shifted(range: &Range<usize>, shift: isize) -> Range<usize> {
    let start = range.start.saturating_add_signed(shift);
    start..start + range.len()
}

/// The best position of one block of lines that may slide between `lower`
/// and `upper` on its own side, and how far it moved.
fn slide(
    block: &Range<usize>,
    lower: usize,
    upper: usize,
    side: &mut Side<'_>,
) -> Option<(Range<usize>, isize)> {
    let len = block.len();
    let mut start = block.start;
    while start > lower && side.same_line(start - 1, start + len - 1) {
        start -= 1;
    }
    let mut best: Option<(usize, usize)> = None;
    let mut candidate = start;
    loop {
        let touched = side.own_in(candidate..candidate + len).len();
        let distance = candidate.abs_diff(block.start);
        let better = match best {
            None => true,
            Some((score, at)) => {
                touched < score || (touched == score && distance < at.abs_diff(block.start))
            }
        };
        if better {
            best = Some((touched, candidate));
        }
        if candidate + len >= upper || !side.same_line(candidate, candidate + len) {
            break;
        }
        candidate += 1;
    }
    let (_, at) = best?;
    (at != block.start).then(|| {
        let shift = at as isize - block.start as isize;
        (at..at + len, shift)
    })
}

struct Decide<'a> {
    chosen: &'a HashSet<EntityId>,
    carry_outside: bool,
    names: &'a HashMap<EntityId, String>,
}

impl Decide<'_> {
    fn hunk(
        &self,
        hunk: &Hunk,
        from: &mut Side<'_>,
        to: &mut Side<'_>,
        out: &mut Vec<u8>,
    ) -> Result<(), Entangled> {
        let mut parts = from.own_in(hunk.from.clone());
        parts.extend(to.own_in(hunk.to.clone()));
        if parts.is_empty() {
            if self.carry_outside {
                to.push_lines(out, hunk.to.clone());
            } else {
                from.push_lines(out, hunk.from.clone());
            }
            return Ok(());
        }
        let chosen = parts.iter().filter(|id| self.chosen.contains(id)).count();
        if chosen == parts.len() {
            to.push_lines(out, hunk.to.clone());
            return Ok(());
        }
        if chosen == 0 {
            from.push_lines(out, hunk.from.clone());
            return Ok(());
        }
        self.separate(hunk, from, to, out, &parts)
    }

    /// Carry part of one region whose lines belong to chosen and unchosen
    /// entities alike, one entity's lines at a time.
    fn separate(
        &self,
        hunk: &Hunk,
        from: &mut Side<'_>,
        to: &mut Side<'_>,
        out: &mut Vec<u8>,
        parts: &BTreeSet<EntityId>,
    ) -> Result<(), Entangled> {
        let from_groups = from.groups(hunk.from.clone());
        let to_groups = to.groups(hunk.to.clone());
        // An entity present on both sides of the region is one change, decided
        // once over everything it touches on either side.
        let mut owned: HashMap<EntityId, BTreeSet<EntityId>> = HashMap::new();
        for group in from_groups.iter().chain(&to_groups) {
            if let Some(owner) = group.owner {
                owned
                    .entry(owner)
                    .or_default()
                    .extend(group.own.iter().copied());
            }
        }
        let mut take: HashMap<EntityId, bool> = HashMap::new();
        for (owner, own) in &owned {
            take.insert(*owner, self.decide_owned(own)?);
        }
        let paired = |owner: EntityId| from_groups.iter().position(|g| g.owner == Some(owner));
        let in_to = |owner: EntityId| to_groups.iter().any(|g| g.owner == Some(owner));

        let mut pieces: Vec<Piece> = Vec::new();
        let mut next_from = 0;
        let keep_from = |group: &Group, pieces: &mut Vec<Piece>| {
            let keep = match group.owner {
                Some(owner) => !take[&owner],
                None => !self.carry_outside,
            };
            if keep {
                pieces.push(Piece {
                    origin: Origin::From,
                    lines: group.lines.clone(),
                    alone: group.owner.is_some_and(|owner| !in_to(owner)),
                });
            }
        };
        // What this version already holds stays where it stands, and what is
        // carried from the other version follows it, between the entities both
        // versions hold.
        let mut carried: Vec<Piece> = Vec::new();
        for group in &to_groups {
            match group.owner {
                Some(owner) => {
                    if let Some(at) = paired(owner) {
                        if at < next_from {
                            return Err(self.entangled(parts));
                        }
                        for skipped in &from_groups[next_from..at] {
                            keep_from(skipped, &mut pieces);
                        }
                        pieces.append(&mut carried);
                        next_from = at + 1;
                        pieces.push(if take[&owner] {
                            Piece {
                                origin: Origin::To,
                                lines: group.lines.clone(),
                                alone: false,
                            }
                        } else {
                            Piece {
                                origin: Origin::From,
                                lines: from_groups[at].lines.clone(),
                                alone: false,
                            }
                        });
                    } else if take[&owner] {
                        carried.push(Piece {
                            origin: Origin::To,
                            lines: group.lines.clone(),
                            alone: true,
                        });
                    }
                }
                None => {
                    if self.carry_outside {
                        carried.push(Piece {
                            origin: Origin::To,
                            lines: group.lines.clone(),
                            alone: false,
                        });
                    }
                }
            }
        }
        for group in &from_groups[next_from..] {
            keep_from(group, &mut pieces);
        }
        pieces.append(&mut carried);

        // An entity one version holds alone, written where the other version
        // holds a different one, never stood beside it in either version, so
        // no spacing between the two exists to carry. They take the spacing
        // that separates this region from what stands above it.
        let spacing = {
            let above = from.spacing_above(hunk.from.start);
            if above.is_empty() {
                to.spacing_above(hunk.to.start)
            } else {
                above
            }
        };
        let mut previous: Option<&Piece> = None;
        for piece in &pieces {
            let side: &Side<'_> = match piece.origin {
                Origin::From => from,
                Origin::To => to,
            };
            if let Some(before) = previous {
                let before_side: &Side<'_> = match before.origin {
                    Origin::From => from,
                    Origin::To => to,
                };
                if out.last().is_some_and(|byte| *byte != b'\n') {
                    out.push(b'\n');
                }
                if before.origin != piece.origin
                    && (before.alone || piece.alone)
                    && !before_side.reading_cached(before.lines.end - 1).blank
                    && !side.reading_cached(piece.lines.start).blank
                {
                    out.extend_from_slice(&spacing);
                }
            }
            // Spacing that separated an entity from one above it has nothing
            // to separate when the entity opens the content.
            let mut lines = piece.lines.clone();
            if out.is_empty() && piece.alone {
                while lines.len() > 1 && side.reading_cached(lines.start).blank {
                    lines.start += 1;
                }
            }
            side.push_lines(out, lines);
            previous = Some(piece);
        }
        Ok(())
    }

    /// Whether one entity's lines are carried: all of what they touch chosen,
    /// or none of it.
    fn decide_owned(&self, own: &BTreeSet<EntityId>) -> Result<bool, Entangled> {
        let chosen = own.iter().find(|id| self.chosen.contains(id));
        let other = own.iter().find(|id| !self.chosen.contains(id));
        match (chosen, other) {
            (Some(chosen), Some(other)) => Err(Entangled {
                chosen: self.name(chosen),
                other: self.name(other),
            }),
            (Some(_), None) => Ok(true),
            _ => Ok(false),
        }
    }

    fn entangled(&self, parts: &BTreeSet<EntityId>) -> Entangled {
        let chosen = parts.iter().find(|id| self.chosen.contains(id));
        let other = parts.iter().find(|id| !self.chosen.contains(id));
        Entangled {
            chosen: chosen.map(|id| self.name(id)).unwrap_or_default(),
            other: other.map(|id| self.name(id)).unwrap_or_default(),
        }
    }

    fn name(&self, id: &EntityId) -> String {
        self.names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entity of `text` running from the first `start` to the end of the
    /// first `end` after it.
    fn spanned(text: &str, name: &str, start: &str, end: &str) -> Spanned {
        let at = text
            .find(start)
            .unwrap_or_else(|| panic!("{start} in {text}"));
        let close = text[at..]
            .find(end)
            .unwrap_or_else(|| panic!("{end} in {text}"))
            + at;
        Spanned {
            id: id(name),
            name: name.to_string(),
            kind: EntityKind::Function,
            span: at..close + end.len(),
        }
    }

    fn function(text: &str, name: &str) -> Spanned {
        spanned(text, name, &format!("fn {name}("), "\n}")
    }

    fn id(name: &str) -> EntityId {
        EntityId::from_content("t.rs", name, "Function", 0)
    }

    fn module(text: &str) -> Spanned {
        Spanned {
            id: id("module"),
            name: "module".into(),
            kind: EntityKind::Module,
            span: 0..text.len(),
        }
    }

    fn versioned(text: &str, names: &[&str]) -> Vec<Spanned> {
        let mut entities = vec![module(text)];
        entities.extend(names.iter().map(|name| function(text, name)));
        entities
    }

    fn keep(
        from: &str,
        from_names: &[&str],
        to: &str,
        to_names: &[&str],
        chosen: &[&str],
    ) -> String {
        keep_with(
            from,
            from_names,
            to,
            to_names,
            chosen,
            Outside::WhenEveryEntityIsChosen,
        )
        .expect("the chosen entities separate")
    }

    fn keep_with(
        from: &str,
        from_names: &[&str],
        to: &str,
        to_names: &[&str],
        chosen: &[&str],
        outside: Outside,
    ) -> Result<String, Entangled> {
        let from_entities = versioned(from, from_names);
        let to_entities = versioned(to, to_names);
        let chosen = chosen.iter().map(|name| id(name)).collect::<HashSet<_>>();
        select(
            Version {
                bytes: from.as_bytes(),
                entities: &from_entities,
            },
            Version {
                bytes: to.as_bytes(),
                entities: &to_entities,
            },
            &chosen,
            outside,
        )
        .map(|selected| String::from_utf8(selected.bytes).unwrap())
    }

    const HEAD: &str =
        "use a;\n\nfn one() {\n    1\n}\n\nfn two() {\n    2\n}\n\nfn three() {\n    3\n}\n";

    #[test]
    fn a_chosen_edit_is_carried_and_an_unchosen_one_is_left() {
        let work = HEAD
            .replace("    1\n", "    10\n")
            .replace("    3\n", "    30\n");
        let names = ["one", "two", "three"];
        assert_eq!(
            keep(HEAD, &names, &work, &names, &["one"]),
            HEAD.replace("    1\n", "    10\n")
        );
        assert_eq!(
            keep(HEAD, &names, &work, &names, &["three"]),
            HEAD.replace("    3\n", "    30\n")
        );
        assert_eq!(keep(HEAD, &names, &work, &names, &["one", "three"]), work);
    }

    #[test]
    fn an_added_entity_is_inserted_where_it_was_written_and_left_out_when_unchosen() {
        let work = HEAD.replace(
            "fn two() {\n    2\n}\n",
            "fn two() {\n    2\n}\n\nfn added() {\n    4\n}\n",
        );
        let to_names = ["one", "two", "added", "three"];
        assert_eq!(
            keep(HEAD, &["one", "two", "three"], &work, &to_names, &["added"]),
            work
        );
        let edited = work.replace("    1\n", "    10\n");
        assert_eq!(
            keep(HEAD, &["one", "two", "three"], &edited, &to_names, &["one"]),
            HEAD.replace("    1\n", "    10\n"),
            "the new entity stays uncommitted when it is not chosen"
        );
    }

    #[test]
    fn two_entities_added_side_by_side_are_kept_one_at_a_time() {
        let work = HEAD.replace(
            "fn three() {",
            "fn left() {\n    5\n}\n\nfn right() {\n    6\n}\n\nfn three() {",
        );
        let from = ["one", "two", "three"];
        let to = ["one", "two", "left", "right", "three"];
        assert_eq!(
            keep(HEAD, &from, &work, &to, &["right"]),
            HEAD.replace("fn three() {", "fn right() {\n    6\n}\n\nfn three() {")
        );
        assert_eq!(
            keep(HEAD, &from, &work, &to, &["left"]),
            HEAD.replace("fn three() {", "fn left() {\n    5\n}\n\nfn three() {")
        );
    }

    #[test]
    fn a_removed_entity_is_removed_only_when_chosen() {
        let work = HEAD.replace("fn two() {\n    2\n}\n\n", "");
        let from = ["one", "two", "three"];
        let to = ["one", "three"];
        assert_eq!(keep(HEAD, &from, &work, &to, &["two"]), work);
        let edited = work.replace("    1\n", "    10\n");
        assert_eq!(
            keep(HEAD, &from, &edited, &to, &["one"]),
            HEAD.replace("    1\n", "    10\n"),
            "the removal stays uncommitted when it is not chosen"
        );
    }

    fn one_liners(text: &str) -> Vec<Spanned> {
        vec![
            module(text),
            spanned(text, "one", "fn one(", "}"),
            spanned(text, "two", "fn two(", "}"),
        ]
    }

    fn select_one_liners(head: &str, work: &str, chosen: &[&str]) -> Result<String, Entangled> {
        let (from, to) = (one_liners(head), one_liners(work));
        let chosen = chosen.iter().map(|name| id(name)).collect::<HashSet<_>>();
        select(
            Version {
                bytes: head.as_bytes(),
                entities: &from,
            },
            Version {
                bytes: work.as_bytes(),
                entities: &to,
            },
            &chosen,
            Outside::WhenEveryEntityIsChosen,
        )
        .map(|selected| String::from_utf8(selected.bytes).unwrap())
    }

    #[test]
    fn edits_on_one_line_with_an_unchosen_entity_are_refused() {
        let refused = select_one_liners(
            "fn one() { 1 } fn two() { 2 }\n",
            "fn one() { 10 } fn two() { 20 }\n",
            &["one"],
        )
        .expect_err("one line holds both entities' changes");
        assert_eq!(refused.chosen, "one");
        assert_eq!(refused.other, "two");
    }

    #[test]
    fn edits_on_adjacent_lines_of_two_entities_separate_by_line() {
        assert_eq!(
            select_one_liners(
                "fn one() { 1 }\nfn two() { 2 }\n",
                "fn one() { 10 }\nfn two() { 20 }\n",
                &["two"],
            )
            .unwrap(),
            "fn one() { 1 }\nfn two() { 20 }\n"
        );
    }

    #[test]
    fn text_outside_every_entity_follows_only_a_content_kept_whole() {
        let work = HEAD
            .replace("use a;\n", "use a;\nuse b;\n")
            .replace("    1\n", "    10\n")
            .replace("    2\n", "    20\n");
        let names = ["one", "two", "three"];
        assert_eq!(
            keep(HEAD, &names, &work, &names, &["one"]),
            HEAD.replace("    1\n", "    10\n"),
            "the import stays with the work that is left"
        );
        assert_eq!(
            keep(HEAD, &names, &work, &names, &["one", "two"]),
            work,
            "a content whose every changed entity is kept is kept whole"
        );
        assert_eq!(
            keep_with(HEAD, &names, &work, &names, &["one", "two"], Outside::Never).unwrap(),
            HEAD.replace("    1\n", "    10\n")
                .replace("    2\n", "    20\n"),
            "and never under the rule that never carries it"
        );
    }

    #[test]
    fn an_insertion_that_matches_a_neighbours_closing_line_is_read_as_the_new_entity() {
        let head = "fn one() {\n    1\n}\n";
        let work = "fn one() {\n    1\n}\n\nfn two() {\n    1\n}\n";
        let kept = keep(head, &["one"], work, &["one", "two"], &["two"]);
        assert_eq!(kept, work);
        let left = keep_with(
            head,
            &["one"],
            &work.replace("fn one() {\n    1", "fn one() {\n    9"),
            &["one", "two"],
            &["one"],
            Outside::WhenEveryEntityIsChosen,
        )
        .unwrap();
        assert_eq!(left, "fn one() {\n    9\n}\n");
    }

    #[test]
    fn a_new_content_keeps_only_the_chosen_entity() {
        let work = "use z;\n\nfn left() {\n    1\n}\n\nfn right() {\n    2\n}\n";
        let kept = keep("", &[], work, &["left", "right"], &["right"]);
        assert_eq!(kept, "fn right() {\n    2\n}\n");
        assert_eq!(
            keep("", &[], work, &["left", "right"], &["left"]),
            "fn left() {\n    1\n}\n"
        );
        assert_eq!(
            keep("", &[], work, &["left", "right"], &["left", "right"]),
            work
        );
    }

    #[test]
    fn an_entity_written_in_place_of_another_takes_the_spacing_above_them() {
        // The working copy replaced `three` with `four` in the same place.
        let work = HEAD.replace("fn three() {\n    3\n}\n", "fn four() {\n    4\n}\n");
        let from = ["one", "two", "three"];
        let to = ["one", "two", "four"];
        assert_eq!(
            keep(HEAD, &from, &work, &to, &["four"]),
            format!("{HEAD}\nfn four() {{\n    4\n}}\n"),
            "kept alone, the new entity stands beside the one whose removal was not chosen"
        );
        assert_eq!(
            keep(HEAD, &from, &work, &to, &["three"]),
            HEAD.replace("fn three() {\n    3\n}\n", ""),
            "and the removal alone leaves neither"
        );
        assert_eq!(keep(HEAD, &from, &work, &to, &["three", "four"]), work);
    }

    #[test]
    fn undoing_one_entity_reverses_its_change_and_leaves_the_others() {
        // Undo is a selection in the other direction: from the working copy
        // toward the recorded version, with only the undone entity chosen.
        let work = HEAD
            .replace("    1\n", "    10\n")
            .replace("fn two() {\n    2\n}\n\n", "")
            .replace("use a;\n", "use a;\nuse b;\n");
        let undone = keep_with(
            &work,
            &["one", "three"],
            HEAD,
            &["one", "two", "three"],
            &["two"],
            Outside::Never,
        )
        .unwrap();
        assert_eq!(
            undone,
            work.replace("fn three() {", "fn two() {\n    2\n}\n\nfn three() {"),
            "the removed entity comes back where it stood and nothing else moves"
        );
    }
}
