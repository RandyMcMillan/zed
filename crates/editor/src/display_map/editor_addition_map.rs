#![allow(unused)]
// TODO kb

use std::ops::{Add, AddAssign, Range, Sub};

use crate::MultiBufferSnapshot;

use super::{
    suggestion_map::{SuggestionEdit, SuggestionPoint, SuggestionSnapshot},
    TextHighlights,
};
use gpui::fonts::HighlightStyle;
use language::{Chunk, Edit, Point, TextSummary};
use rand::Rng;
use sum_tree::Bias;

pub struct EditorAdditionMap;

#[derive(Clone)]
pub struct EditorAdditionSnapshot {
    // TODO kb merge these two together
    pub suggestion_snapshot: SuggestionSnapshot,
    pub version: usize,
}

pub type EditorAdditionEdit = Edit<EditorAdditionOffset>;

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct EditorAdditionOffset(pub usize);

impl Add for EditorAdditionOffset {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for EditorAdditionOffset {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl AddAssign for EditorAdditionOffset {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct EditorAdditionPoint(pub Point);

#[derive(Clone)]
pub struct EditorAdditionBufferRows<'a> {
    _z: &'a std::marker::PhantomData<()>,
}

#[derive(Clone)]
pub struct EditorAdditionChunks<'a> {
    _z: &'a std::marker::PhantomData<()>,
}

impl<'a> Iterator for EditorAdditionChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!("TODO kb")
    }
}

impl<'a> Iterator for EditorAdditionBufferRows<'a> {
    type Item = Option<u32>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!("TODO kb")
    }
}

impl EditorAdditionPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }

    pub fn row(self) -> u32 {
        self.0.row
    }

    pub fn column(self) -> u32 {
        self.0.column
    }
}

impl EditorAdditionMap {
    pub fn new(suggestion_snapshot: SuggestionSnapshot) -> (Self, EditorAdditionSnapshot) {
        todo!("TODO kb")
    }

    pub fn sync(
        &self,
        suggestion_snapshot: SuggestionSnapshot,
        suggestion_edits: Vec<SuggestionEdit>,
    ) -> (EditorAdditionSnapshot, Vec<EditorAdditionEdit>) {
        todo!("TODO kb")
    }

    pub fn randomly_mutate(
        &self,
        rng: &mut impl Rng,
    ) -> (EditorAdditionSnapshot, Vec<EditorAdditionEdit>) {
        todo!("TODO kb")
    }
}

impl EditorAdditionSnapshot {
    pub fn buffer_snapshot(&self) -> &MultiBufferSnapshot {
        todo!("TODO kb")
    }

    pub fn to_point(&self, offset: EditorAdditionOffset) -> EditorAdditionPoint {
        todo!("TODO kb")
    }

    pub fn max_point(&self) -> EditorAdditionPoint {
        todo!("TODO kb")
    }

    pub fn to_offset(&self, point: EditorAdditionPoint) -> EditorAdditionOffset {
        todo!("TODO kb")
    }

    pub fn chars_at(&self, start: EditorAdditionPoint) -> impl '_ + Iterator<Item = char> {
        Vec::new().into_iter()
    }

    pub fn to_suggestion_point(&self, point: EditorAdditionPoint, bias: Bias) -> SuggestionPoint {
        todo!("TODO kb")
    }

    pub fn to_editor_addition_point(&self, point: SuggestionPoint) -> EditorAdditionPoint {
        todo!("TODO kb")
    }

    pub fn clip_point(&self, point: EditorAdditionPoint, bias: Bias) -> EditorAdditionPoint {
        todo!("TODO kb")
    }

    pub fn text_summary_for_range(&self, range: Range<EditorAdditionPoint>) -> TextSummary {
        todo!("TODO kb")
    }

    pub fn buffer_rows<'a>(&'a self, row: u32) -> EditorAdditionBufferRows<'a> {
        todo!("TODO kb")
    }

    pub fn line_len(&self, row: u32) -> u32 {
        todo!("TODO kb")
    }

    pub fn chunks<'a>(
        &'a self,
        range: Range<EditorAdditionOffset>,
        language_aware: bool,
        text_highlights: Option<&'a TextHighlights>,
        suggestion_highlight: Option<HighlightStyle>,
    ) -> EditorAdditionChunks<'a> {
        todo!("TODO kb")
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        todo!("TODO kb")
    }
}
