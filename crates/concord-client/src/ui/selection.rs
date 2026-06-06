//! Discord-style cross-message text selection for the chat log.
//!
//! gpui has no built-in selection that spans separate elements, and
//! gpui-component's text widgets only select within a single widget, so a drag
//! can't cross from one message into the next. This module layers a custom
//! selection over the virtualized message list instead:
//!
//! - Each message body renders through [`SelectableText`], a thin element that
//!   wraps a real [`StyledText`] (so gpui still shapes and paints the text) and,
//!   after layout, stashes a clone of the text's [`TextLayout`] into a shared
//!   [`registry`](SelectionState::registry) keyed by message id. Because
//!   `TextLayout` is an `Rc<RefCell<…>>`, the stashed clone observes the same
//!   laid-out lines and bounds the row painted, which is what makes pixel ⇄
//!   char-offset hit-testing possible from the view.
//! - [`SelectionState`] holds the drag as an `anchor` and `head`, each a
//!   `(message id, byte offset)`. The view updates `head` on drag and reads the
//!   registry to turn a pointer into `(message, offset)` via [`hit_test`].
//! - The selected sub-range of each affected row is handed back to its
//!   `SelectableText` as a highlight, drawn with gpui's own text-run background
//!   (no manual quads).
//! - Copying walks the rows anchor → head in render order and joins their plain
//!   bodies with newlines.
//!
//! Selection is pure view state: it never touches the list's item set, the
//! splice diff, or the row model, so tail-follow and scroll-back paging are
//! unaffected. Rows that scroll out simply stop registering; the `(id, offset)`
//! endpoints survive, so scrolling back re-highlights.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, HighlightStyle, Hsla, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, SharedString, StyledText, TextLayout, Window,
};
use uuid::Uuid;

/// The laid-out text of one rendered message, captured each frame so the view
/// can hit-test a pointer against it. The `layout` is a handle shared with the
/// row's painted element (an `Rc<RefCell<…>>` under the hood), so reading it
/// reflects the most recent layout.
#[derive(Clone)]
pub struct RowText {
    pub layout: TextLayout,
    pub content: SharedString,
}

/// The shared map from message id to its laid-out text, refilled every frame by
/// the mounted [`SelectableText`] rows and read by the view on mouse events.
pub type Registry = Rc<RefCell<HashMap<Uuid, RowText>>>;

/// A point in the chat text: a message and a byte offset into its body. Byte
/// offsets always land on char boundaries (gpui returns boundary indices).
pub type Caret = (Uuid, usize);

/// The selection's endpoints ordered by render position — `start` precedes
/// `end` — so a row's highlight can be computed without holding the live
/// [`SelectionState`] (the per-row render closure is `'static`).
#[derive(Clone, Copy)]
pub struct OrderedSelection {
    pub start: Caret,
    pub end: Caret,
}

impl OrderedSelection {
    /// The selected byte range within `msg_id`'s body, given the render `order`
    /// and that message's full `len`. `None` when the message lies outside the
    /// selection. Fully-covered middle messages return `0..len`; the boundary
    /// messages return their partial range.
    pub fn range_in_row(&self, msg_id: Uuid, len: usize, order: &[Uuid]) -> Option<Range<usize>> {
        let idx = order.iter().position(|id| *id == msg_id)?;
        let si = order.iter().position(|id| *id == self.start.0)?;
        let ei = order.iter().position(|id| *id == self.end.0)?;
        if idx < si || idx > ei {
            return None;
        }
        let from = if idx == si { self.start.1 } else { 0 };
        let to = if idx == ei { self.end.1 } else { len };
        let (from, to) = (from.min(len), to.min(len));
        (from < to).then_some(from..to)
    }
}

/// The active drag selection over the message list, plus the per-frame registry
/// of laid-out rows it hit-tests against. Owned by the root view as plain view
/// state.
#[derive(Default)]
pub struct SelectionState {
    /// Where the drag began, as `(message, offset)`.
    anchor: Option<Caret>,
    /// Where the drag currently is, as `(message, offset)`. Equal to `anchor`
    /// for a plain (zero-length) click, which reads as no selection.
    head: Option<Caret>,
    /// Laid-out rows, keyed by message id, refilled each frame.
    registry: Registry,
}

impl SelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared registry handle to hand to each [`SelectableText`].
    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    /// Forget every laid-out row. Called at the top of each list render so rows
    /// that unmounted on scroll drop out and only mounted rows are hit-tested.
    pub fn clear_registry(&self) {
        self.registry.borrow_mut().clear();
    }

    /// Begin a drag at `caret` (or clear, if the pointer hit no text).
    pub fn begin(&mut self, caret: Option<Caret>) {
        self.anchor = caret;
        self.head = caret;
    }

    /// Extend the drag to `caret`, keeping the anchor fixed.
    pub fn extend(&mut self, caret: Option<Caret>) {
        if self.anchor.is_some() {
            if let Some(caret) = caret {
                self.head = Some(caret);
            }
        }
    }

    /// Drop the selection entirely.
    pub fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
    }

    /// Whether a non-empty selection exists. A zero-length selection (anchor ==
    /// head, e.g. a plain click) counts as inactive so clicking clears.
    pub fn is_active(&self) -> bool {
        matches!((self.anchor, self.head), (Some(a), Some(h)) if a != h)
    }

    /// The anchor and head ordered by their position in `order` (render order),
    /// so `start` precedes `end` regardless of drag direction. `None` unless the
    /// selection is active and both endpoints are present in `order`.
    pub fn ordered(&self, order: &[Uuid]) -> Option<OrderedSelection> {
        let (anchor, head) = match (self.anchor, self.head) {
            (Some(a), Some(h)) if a != h => (a, h),
            _ => return None,
        };
        let ai = order.iter().position(|id| *id == anchor.0)?;
        let hi = order.iter().position(|id| *id == head.0)?;
        // Earlier row first; within the same row, smaller offset first.
        let (start, end) = if (ai, anchor.1) <= (hi, head.1) {
            (anchor, head)
        } else {
            (head, anchor)
        };
        Some(OrderedSelection { start, end })
    }

    /// The selected byte range within `msg_id`'s body, given the render `order`
    /// and that message's full `len`. `None` when the message lies outside the
    /// selection.
    pub fn range_for(&self, msg_id: Uuid, len: usize, order: &[Uuid]) -> Option<Range<usize>> {
        self.ordered(order)?.range_in_row(msg_id, len, order)
    }

    /// Assemble the selected text: the plain message bodies from anchor to head
    /// in render order, the boundary rows sliced to their partial ranges and the
    /// middle rows taken whole, joined by newlines. Empty when nothing is
    /// selected. Rows missing from the registry (scrolled off) are skipped for
    /// their *content* but never split a covered range — their full body is used
    /// only when we know its length, so an unmounted middle row contributes an
    /// empty line rather than a wrong slice.
    pub fn selected_text(&self, order: &[Uuid]) -> String {
        let Some(OrderedSelection { start, end }) = self.ordered(order) else {
            return String::new();
        };
        let registry = self.registry.borrow();
        let si = order.iter().position(|id| *id == start.0);
        let ei = order.iter().position(|id| *id == end.0);
        let (Some(si), Some(ei)) = (si, ei) else {
            return String::new();
        };

        let mut pieces: Vec<String> = Vec::new();
        for id in &order[si..=ei] {
            let content = registry.get(id).map(|r| r.content.as_ref()).unwrap_or("");
            let len = content.len();
            let from = if *id == start.0 { start.1 } else { 0 };
            let to = if *id == end.0 { end.1 } else { len };
            let (from, to) = (from.min(len), to.min(len));
            pieces.push(content.get(from..to).unwrap_or("").to_string());
        }
        pieces.join("\n")
    }
}

/// Hit-test a window-space pointer to `(message, byte offset)` against the rows
/// laid out in `registry`, walking them in render `order`.
///
/// A pointer inside a row's text bounds maps through that row's
/// [`TextLayout::index_for_position`]. Above the first or below the last laid-out
/// row, it clamps to that row's start / end. Between two rows it picks the nearer
/// one and clamps to its near edge, so a drag through the gutter or the gap
/// between messages still spans cleanly.
pub fn hit_test(
    pos: Point<Pixels>,
    order: &[Uuid],
    registry: &Registry,
) -> Option<Caret> {
    let registry = registry.borrow();

    // The mounted rows in render order, with their current bounds. Every entry
    // in the registry was inserted during prepaint, so its layout has been
    // measured and its bounds are set — `bounds()` is safe here.
    let mut laid: Vec<(Uuid, Bounds<Pixels>, &RowText)> = Vec::new();
    for id in order {
        if let Some(row) = registry.get(id) {
            laid.push((*id, row.layout.bounds(), row));
        }
    }
    let first = laid.first()?;

    // Above everything: clamp to the top row's start.
    if pos.y < first.1.top() {
        return Some((first.0, 0));
    }

    for (i, (id, bounds, row)) in laid.iter().enumerate() {
        if pos.y <= bounds.bottom() {
            // Inside (or to the side of) this row's vertical band: let the text
            // layout resolve the offset, clamping a miss to its nearest index.
            let offset = match row.layout.index_for_position(pos) {
                Ok(ix) | Err(ix) => ix,
            };
            return Some((*id, offset));
        }
        // In the gap below this row but above the next one: snap to whichever
        // edge is nearer.
        if let Some((next_id, next_bounds, _)) = laid.get(i + 1) {
            if pos.y < next_bounds.top() {
                let mid = (bounds.bottom() + next_bounds.top()) / 2.0;
                return Some(if pos.y <= mid {
                    (*id, row.content.len())
                } else {
                    (*next_id, 0)
                });
            }
        }
    }

    // Below everything: clamp to the last row's end.
    let last = laid.last()?;
    Some((last.0, last.2.content.len()))
}

/// A message body that participates in cross-message selection.
///
/// It wraps a [`StyledText`] (so gpui owns the shaping, wrapping, and painting)
/// and does two extra things: it paints the selected sub-range with a background
/// highlight, and after layout it publishes a clone of the text's [`TextLayout`]
/// into the shared registry so the view can hit-test pointers against it.
pub struct SelectableText {
    msg_id: Uuid,
    content: SharedString,
    text: StyledText,
    registry: Registry,
}

impl SelectableText {
    /// Build a selectable body for `msg_id`. `highlight` is the selected byte
    /// range within `content`, drawn with `selection_color`; `None` for an
    /// unselected row.
    pub fn new(
        msg_id: Uuid,
        content: SharedString,
        highlight: Option<Range<usize>>,
        selection_color: Hsla,
        registry: Registry,
    ) -> Self {
        let mut text = StyledText::new(content.clone());
        if let Some(range) = highlight {
            let style = HighlightStyle {
                background_color: Some(selection_color),
                ..Default::default()
            };
            text = text.with_highlights([(range, style)]);
        }
        Self { msg_id, content, text, registry }
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text.prepaint(id, inspector_id, bounds, state, window, cx);
        // The child's layout now knows its bounds; publish a handle so the view
        // can hit-test it. The clone shares the same `Rc<RefCell>` storage.
        self.registry.borrow_mut().insert(
            self.msg_id,
            RowText {
                layout: self.text.layout().clone(),
                content: self.content.clone(),
            },
        );
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut (),
        prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector_id, bounds, request, prepaint, window, cx);
    }
}

#[cfg(test)]
mod tests {
    // Import specifics only: a `use super::*` here re-globs gpui's prelude and
    // trips the gpui macro recursion limit in this crate's test build.
    use super::SelectionState;
    use uuid::Uuid;

    /// Three stable message ids in render order.
    fn ids() -> (Uuid, Uuid, Uuid) {
        (
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
        )
    }

    /// Drive a `SelectionState` directly, populating its registry with the given
    /// bodies so `selected_text` has content to slice.
    fn state_with(bodies: &[(Uuid, &str)]) -> SelectionState {
        let state = SelectionState::new();
        {
            let registry = state.registry();
            let mut reg = registry.borrow_mut();
            for (id, body) in bodies {
                reg.insert(
                    *id,
                    super::RowText {
                        // A default TextLayout is fine: the copy/range logic
                        // only reads `content`, never the (unlaid) layout.
                        layout: gpui::TextLayout::default(),
                        content: gpui::SharedString::from(body.to_string()),
                    },
                );
            }
        }
        state
    }

    #[test]
    fn zero_length_selection_is_inactive() {
        let (a, _, _) = ids();
        let mut s = SelectionState::new();
        s.begin(Some((a, 3)));
        assert!(!s.is_active(), "a plain click must read as no selection");
        assert_eq!(s.selected_text(&[a]), "");
    }

    #[test]
    fn single_message_partial_range() {
        let (a, _, _) = ids();
        let order = [a];
        let mut s = state_with(&[(a, "hello world")]);
        s.begin(Some((a, 0)));
        s.extend(Some((a, 5)));
        assert!(s.is_active());
        assert_eq!(s.range_for(a, "hello world".len(), &order), Some(0..5));
        assert_eq!(s.selected_text(&order), "hello");
    }

    #[test]
    fn spans_three_messages_forward() {
        let (a, b, c) = ids();
        let order = [a, b, c];
        let mut s = state_with(&[(a, "first one"), (b, "middle"), (c, "last line")]);
        // Drag from mid-first to mid-last.
        s.begin(Some((a, 6)));
        s.extend(Some((c, 4)));

        // Boundary rows partial, middle row whole.
        assert_eq!(s.range_for(a, "first one".len(), &order), Some(6..9));
        assert_eq!(s.range_for(b, "middle".len(), &order), Some(0..6));
        assert_eq!(s.range_for(c, "last line".len(), &order), Some(0..4));
        assert_eq!(s.selected_text(&order), "one\nmiddle\nlast");
    }

    #[test]
    fn drag_backwards_normalizes() {
        let (a, b, c) = ids();
        let order = [a, b, c];
        let mut s = state_with(&[(a, "first one"), (b, "middle"), (c, "last line")]);
        // Anchor in the last row, head in the first — reverse drag.
        s.begin(Some((c, 4)));
        s.extend(Some((a, 6)));
        assert_eq!(s.selected_text(&order), "one\nmiddle\nlast");
        assert_eq!(s.range_for(b, "middle".len(), &order), Some(0..6));
    }

    #[test]
    fn message_outside_selection_has_no_range() {
        let (a, b, c) = ids();
        let order = [a, b, c];
        let mut s = state_with(&[(a, "first one"), (b, "middle"), (c, "last line")]);
        s.begin(Some((a, 0)));
        s.extend(Some((b, 3)));
        // c is past the head, so it isn't highlighted.
        assert_eq!(s.range_for(c, "last line".len(), &order), None);
    }

    #[test]
    fn clear_drops_selection() {
        let (a, b, _) = ids();
        let order = [a, b];
        let mut s = state_with(&[(a, "first one"), (b, "middle")]);
        s.begin(Some((a, 0)));
        s.extend(Some((b, 3)));
        assert!(s.is_active());
        s.clear();
        assert!(!s.is_active());
        assert_eq!(s.selected_text(&order), "");
    }
}
