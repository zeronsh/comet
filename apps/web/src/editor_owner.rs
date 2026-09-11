//! One editor, one owner. Rejected text remains in the editor, never in a draft map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub auth_epoch: u64,
    pub engine: String,
    pub chat: String,
}
#[derive(Default)]
pub struct EditorOwner {
    pub owner: Option<Owner>,
    pub rejected: bool,
}
impl EditorOwner {
    /// False means the caller must preserve the entire existing editor buffer.
    pub fn sync(&mut self, active: Option<Owner>, draft_error: bool) -> bool {
        if self.rejected {
            return false;
        }
        self.owner = active;
        self.rejected = draft_error;
        true
    }
    pub fn can_write(&self, active: &Option<Owner>) -> bool {
        self.owner.is_some() && &self.owner == active
    }
    pub fn edited(&mut self, accepted: bool) {
        self.rejected = !accepted;
    }
    pub fn discard(&mut self) {
        self.rejected = false;
        self.owner = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn owner(chat: &str, epoch: u64) -> Option<Owner> {
        Some(Owner {
            auth_epoch: epoch,
            engine: "engine".into(),
            chat: chat.into(),
        })
    }
    #[test]
    fn rejected_a_b_a_never_adopts_b_text() {
        let mut state = EditorOwner::default();
        let mut text = "valid A";
        assert!(state.sync(owner("A", 0), false));
        state.edited(false);
        text = "oversized A";
        if state.sync(owner("B", 0), false) {
            text = "distinct valid B";
        }
        assert_eq!(text, "oversized A");
        assert!(!state.can_write(&owner("B", 0)));
        if state.sync(owner("A", 0), true) {
            text = "valid A";
        }
        assert_eq!(text, "oversized A");
        assert!(state.can_write(&owner("A", 0)));
        state.edited(true);
        assert!(state.sync(owner("B", 0), false));
    }
    #[test]
    fn automatic_logout_identity_change_preserves_but_cannot_send() {
        let mut state = EditorOwner::default();
        state.sync(owner("A", 0), false);
        state.edited(false);
        assert!(!state.sync(None, false));
        assert!(!state.can_write(&None));
        assert!(!state.sync(owner("A", 1), false));
        assert!(!state.can_write(&owner("A", 1)));
        state.discard();
        assert!(state.sync(owner("A", 1), false));
        assert!(state.can_write(&owner("A", 1)));
    }
}
