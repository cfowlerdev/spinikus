//! Cursor pagination: lists take the ID to start after and a limit, and return a [`Page`].

/// The largest page a list returns, whatever limit is asked for.
pub const MAX_PAGE_SIZE: u32 = 100;

/// One page of a list in ID order. The next page starts after the last item's ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub has_more: bool,
}

/// The limit to use, between 1 and [`MAX_PAGE_SIZE`]. Lists fetch one more item than this,
/// to tell whether there is a next page.
pub(crate) fn limit(requested: u32) -> u32 {
    requested.clamp(1, MAX_PAGE_SIZE)
}

impl<T> Page<T> {
    /// A page from up to `limit + 1` items.
    pub(crate) fn from_overfetch(mut items: Vec<T>, limit: u32) -> Self {
        let has_more = items.len() > limit as usize;
        items.truncate(limit as usize);
        Self { items, has_more }
    }
}
