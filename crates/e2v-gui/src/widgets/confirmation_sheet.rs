pub fn has_pending_confirmation(pending: &Option<crate::domain::PendingConfirmation>) -> bool {
    pending.is_some()
}
