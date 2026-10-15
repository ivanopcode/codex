use crate::session::session::Session;

impl Session {
    /// Queue-only runtime data remains visible to history readers even when it
    /// cannot wake the model. Active turns consume it through the normal drain.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "idle history recording must precede any concurrent turn admission"
    )]
    pub(super) async fn record_idle_runtime_fragments(&self) {
        let active = self.active_turn.lock().await;
        if active.is_some() {
            return;
        }
        let fragments = self
            .input_queue
            .take_runtime_fragments_for_idle_recording()
            .await;
        if fragments.is_empty() {
            return;
        }
        let context = self.new_inject_items_context().await;
        self.record_annotated_conversation_items(&context, context.model_info(), fragments)
            .await;
        drop(active);
    }
}
