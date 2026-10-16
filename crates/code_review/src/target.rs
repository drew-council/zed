use gpui::SharedString;

/// What a review covers. Branch, commit and pull request targets will be added alongside
/// `Uncommitted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewTarget {
    /// HEAD → working tree, staged and unstaged changes together.
    Uncommitted,
}

impl ReviewTarget {
    pub fn title(&self) -> SharedString {
        match self {
            ReviewTarget::Uncommitted => "Uncommitted changes".into(),
        }
    }

    pub fn empty_label(&self) -> SharedString {
        match self {
            ReviewTarget::Uncommitted => "No uncommitted changes".into(),
        }
    }
}
