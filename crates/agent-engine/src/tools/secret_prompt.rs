use std::sync::{Arc, Mutex};

pub use crate::session::{ConfirmScope, PromptKind};

/// UI-only secret prompt plumbing for interactive tools.
///
/// Secrets sent through this channel are never part of tool parameters, tool
/// results, chat messages, or API messages. The TUI owns the input UI and sends
/// only the final secret bytes back to the waiting tool.
#[derive(Clone)]
pub struct SecretPromptHandle {
    tx: tokio::sync::mpsc::UnboundedSender<SecretPromptRequest>,
}

impl SecretPromptHandle {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<SecretPromptRequest>) -> Self {
        Self { tx }
    }

    pub async fn prompt(&self, title: String, prompt: String) -> Option<String> {
        self.prompt_scoped(title, prompt, None).await
    }

    /// Like [`Self::prompt`], carrying the extension tool-call confirm scope
    /// so the dialog can name the source extension(s) and the reach of
    /// "Allow all" before the user answers.
    pub async fn prompt_scoped(
        &self,
        title: String,
        prompt: String,
        confirm_scope: Option<ConfirmScope>,
    ) -> Option<String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = SecretPromptRequest {
            kind: PromptKind::from_title(&title),
            title,
            prompt,
            confirm_scope,
            response_tx,
        };
        self.tx.send(request).ok()?;
        response_rx.await.ok().flatten()
    }
}

pub struct SecretPromptRequest {
    /// `Confirm` renders as a y/n dialog (body visible), `Secret` as a masked
    /// field. Derived from the title by `SecretPromptHandle::prompt`; carried
    /// verbatim from the wire `PromptRequest` by the daemon PromptBridge.
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    /// Extension tool-call confirms only (see [`ConfirmScope`]).
    pub confirm_scope: Option<ConfirmScope>,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

/// Which button of a `Confirm` dialog is focused / was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmChoice {
    /// Answer "y": allow this one call.
    Allow,
    /// Answer [`CONFIRM_ANSWER_ALLOW_ALL`]: allow this call and latch the
    /// source extension(s) so their later confirms are auto-approved. Only
    /// offered for `Confirm tool call` prompts that carry a [`ConfirmScope`]
    /// (not `activate_tools`), and only reachable deliberately: focus the
    /// button (Tab / arrows) and press Enter. No letter key selects it.
    AllowAll,
    /// Answer `None`: deny (the default focus, so a bare Enter fails closed).
    Deny,
}

/// The answer string [`ConfirmChoice::AllowAll`] sends. The runtime's
/// `resolve_before_tool_call_result` matches on it (re-exported there as
/// `runtime::CONFIRM_ANSWER_ALLOW_ALL`); defined here so the tools module
/// does not depend on the runtime module.
pub const CONFIRM_ANSWER_ALLOW_ALL: &str = "always";

/// A `Confirm` dialog ignores every key for this long after it becomes
/// active, so keystrokes already in flight (typing a steer when the dialog
/// pops up) can never answer it.
pub const CONFIRM_ARM_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

pub struct PendingSecretPrompt {
    pub kind: PromptKind,
    pub title: String,
    pub prompt: String,
    pub confirm_scope: Option<ConfirmScope>,
    pub buffer: String,
    /// `Confirm` only: the focused button. Starts on `Deny`.
    pub confirm_focus: ConfirmChoice,
    /// Start of the current input guard (see [`CONFIRM_ARM_DELAY`]): set
    /// when the prompt becomes active, and restarted by every key ignored
    /// while unarmed and when "Allow all" asks for its confirmation.
    pub activated_at: std::time::Instant,
    /// `Confirm` only: Enter was pressed on "Allow all" once; a second
    /// Enter (after a fresh guard) arms it, Esc or moving focus cancels.
    pub allow_all_pending: bool,
    pub response_tx: tokio::sync::oneshot::Sender<Option<String>>,
}

impl PendingSecretPrompt {
    /// `Confirm` only: whether keys are accepted yet. `Secret` prompts are
    /// always armed (they are typed into, not answered by a single key).
    pub fn input_armed(&self) -> bool {
        self.kind != PromptKind::Confirm || self.activated_at.elapsed() >= CONFIRM_ARM_DELAY
    }

    /// Whether the "Allow all" button is offered: only the runtime's
    /// per-tool-call gate (`Confirm tool call`) with a known source scope,
    /// never the `activate_tools` gate (its runtime only accepts y/yes) and
    /// never an unattributed confirm (the latch could not be scoped).
    pub fn offers_allow_all(&self) -> bool {
        self.kind == PromptKind::Confirm
            && self.title == crate::session::types::CONFIRM_PROMPT_TITLE
            && self
                .confirm_scope
                .as_ref()
                .is_some_and(|s| !s.extensions.is_empty())
    }

    /// Buttons in display order.
    pub fn confirm_choices(&self) -> &'static [ConfirmChoice] {
        if self.offers_allow_all() {
            &[ConfirmChoice::Allow, ConfirmChoice::AllowAll, ConfirmChoice::Deny]
        } else {
            &[ConfirmChoice::Allow, ConfirmChoice::Deny]
        }
    }
}

pub struct SecretPromptQueue {
    active: Option<PendingSecretPrompt>,
    pending: std::collections::VecDeque<SecretPromptRequest>,
}

impl Default for SecretPromptQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretPromptQueue {
    pub fn new() -> Self {
        Self {
            active: None,
            pending: std::collections::VecDeque::new(),
        }
    }

    pub fn poll_requests(
        &mut self,
        rx: &Arc<Mutex<tokio::sync::mpsc::UnboundedReceiver<SecretPromptRequest>>>,
    ) {
        if let Ok(mut rx) = rx.lock() {
            while let Ok(req) = rx.try_recv() {
                self.pending.push_back(req);
            }
        }
        self.activate_next();
    }

    fn activate_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        if let Some(req) = self.pending.pop_front() {
            self.active = Some(PendingSecretPrompt {
                kind: req.kind,
                title: req.title,
                prompt: req.prompt,
                confirm_scope: req.confirm_scope,
                buffer: String::new(),
                confirm_focus: ConfirmChoice::Deny,
                activated_at: std::time::Instant::now(),
                allow_all_pending: false,
                response_tx: req.response_tx,
            });
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn active(&self) -> Option<&PendingSecretPrompt> {
        self.active.as_ref()
    }

    pub fn push_char(&mut self, ch: char) {
        if let Some(active) = self.active.as_mut() {
            active.buffer.push(ch);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.buffer.pop();
        }
    }

    /// `Confirm` dialog: move focus to the next / previous button. Focus
    /// does NOT wrap (from the default Deny, "forward" stays put). Moving
    /// cancels a pending "Allow all" confirmation. No-op for `Secret`.
    pub fn move_confirm_focus(&mut self, forward: bool) {
        if let Some(active) = self.active.as_mut() {
            if active.kind != PromptKind::Confirm {
                return;
            }
            let choices = active.confirm_choices();
            let i = choices
                .iter()
                .position(|c| *c == active.confirm_focus)
                .unwrap_or(choices.len() - 1);
            let j = if forward {
                (i + 1).min(choices.len() - 1)
            } else {
                i.saturating_sub(1)
            };
            active.confirm_focus = choices[j];
            active.allow_all_pending = false;
        }
    }

    /// Restart the input guard: the dialog ignores keys for another
    /// [`CONFIRM_ARM_DELAY`] (called for every key ignored while unarmed, so
    /// typing that runs past the window keeps the dialog dead until a pause).
    pub fn restart_input_guard(&mut self) {
        if let Some(a) = self.active.as_mut() {
            a.activated_at = std::time::Instant::now();
        }
    }

    /// `Confirm` dialog: whether "Allow all" is waiting for its second Enter.
    pub fn allow_all_pending(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.allow_all_pending)
    }

    /// Esc while "Allow all" awaits confirmation: cancel just that step.
    /// Returns whether there was one to cancel.
    pub fn cancel_allow_all_pending(&mut self) -> bool {
        match self.active.as_mut() {
            Some(a) if a.allow_all_pending => {
                a.allow_all_pending = false;
                true
            }
            _ => false,
        }
    }

    /// Input ownership moved away while this prompt is on screen: remove the
    /// "Allow all" button (only the owner may arm it).
    pub fn drop_allow_all(&mut self) {
        if let Some(a) = self.active.as_mut() {
            a.confirm_scope = None;
            a.allow_all_pending = false;
            if a.confirm_focus == ConfirmChoice::AllowAll {
                a.confirm_focus = ConfirmChoice::Deny;
            }
        }
    }

    /// `Confirm` dialog: the focused button (`Deny` when no confirm is active).
    pub fn confirm_focus(&self) -> ConfirmChoice {
        self.active
            .as_ref()
            .filter(|a| a.kind == PromptKind::Confirm)
            .map_or(ConfirmChoice::Deny, |a| a.confirm_focus)
    }

    /// Whether the active prompt accepts keys yet (see [`CONFIRM_ARM_DELAY`]).
    pub fn input_armed(&self) -> bool {
        self.active.as_ref().map_or(true, |a| a.input_armed())
    }

    /// Test/harness seam: treat the active prompt as opened long enough ago
    /// that the arming delay has passed. Not compiled into production.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn age_input_guard_for_tests(&mut self, by: std::time::Duration) {
        if let Some(a) = self.active.as_mut() {
            if let Some(t) = a.activated_at.checked_sub(by) {
                a.activated_at = t;
            }
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn arm_now_for_tests(&mut self) {
        if let Some(a) = self.active.as_mut() {
            if let Some(t) = std::time::Instant::now().checked_sub(CONFIRM_ARM_DELAY * 2) {
                a.activated_at = t;
            }
        }
    }

    /// `Confirm` dialog: whether the "Allow all (session)" button is offered.
    pub fn confirm_offers_allow_all(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.offers_allow_all())
    }

    /// `Confirm` dialog: answer with `choice`. `AllowAll` on a prompt that
    /// does not offer it does nothing (never a silent single Allow).
    /// Returns the choice actually sent, or `None` when nothing was sent.
    pub fn confirm_answer(&mut self, choice: ConfirmChoice) -> Option<ConfirmChoice> {
        let active = self.active.as_mut()?;
        if active.kind != PromptKind::Confirm {
            return None;
        }
        if choice == ConfirmChoice::AllowAll && !active.offers_allow_all() {
            return None;
        }
        match choice {
            ConfirmChoice::Deny => self.cancel(),
            ConfirmChoice::Allow | ConfirmChoice::AllowAll => {
                active.buffer.clear();
                active.buffer.push_str(if choice == ConfirmChoice::Allow {
                    "y"
                } else {
                    CONFIRM_ANSWER_ALLOW_ALL
                });
                self.submit();
            }
        }
        Some(choice)
    }

    /// `Confirm` dialog: activate the focused button (Enter). Deny is the
    /// default focus, so an un-navigated Enter denies (fail-closed).
    /// "Allow all" takes two Enters: the first only asks for confirmation
    /// and restarts the input guard; the second (once armed) answers.
    pub fn confirm_activate_focused(&mut self) -> Option<ConfirmChoice> {
        let choice = self.confirm_focus();
        if choice == ConfirmChoice::AllowAll {
            let active = self.active.as_mut()?;
            if !active.offers_allow_all() {
                return None;
            }
            if !active.allow_all_pending {
                active.allow_all_pending = true;
                active.activated_at = std::time::Instant::now();
                return None;
            }
        }
        self.confirm_answer(choice)
    }

    pub fn submit(&mut self) {
        if let Some(mut active) = self.active.take() {
            let secret = std::mem::take(&mut active.buffer);
            let _ = active.response_tx.send(Some(secret));
        }
        self.activate_next();
    }

    pub fn cancel(&mut self) {
        if let Some(mut active) = self.active.take() {
            active.buffer.clear();
            let _ = active.response_tx.send(None);
        }
        self.activate_next();
    }

    /// Drop the active prompt WITHOUT answering (the oneshot is dropped, not
    /// sent): another client resolved it (`PromptResolved` for a prompt this
    /// client did not answer). The next pending prompt activates.
    pub fn dismiss(&mut self) {
        if let Some(mut active) = self.active.take() {
            active.buffer.clear();
            drop(active.response_tx);
        }
        self.activate_next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismiss_drops_without_answering_and_activates_next() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(Mutex::new(rx));
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest { kind: PromptKind::Secret, title: "a".into(), prompt: "p".into(), confirm_scope: None, response_tx: tx1 }).unwrap();
        tx.send(SecretPromptRequest { kind: PromptKind::Secret, title: "b".into(), prompt: "p".into(), confirm_scope: None, response_tx: tx2 }).unwrap();
        let mut q = SecretPromptQueue::new();
        q.poll_requests(&rx);
        assert_eq!(q.active().unwrap().title, "a");
        q.push_char('z');
        q.dismiss();
        // Dropped, never sent: the waiter sees a closed channel (== cancelled).
        assert!(matches!(rx1.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)));
        // The next prompt is active with a fresh buffer.
        assert_eq!(q.active().unwrap().title, "b");
        assert_eq!(q.active().unwrap().buffer, "");
        q.push_char('x');
        q.submit();
        assert_eq!(rx2.try_recv().unwrap(), Some("x".to_string()));
        assert!(!q.is_active());
    }

    fn confirm(title: &str, scope: Option<ConfirmScope>) -> SecretPromptQueue {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(Mutex::new(rx));
        let (response_tx, _rx) = tokio::sync::oneshot::channel();
        tx.send(SecretPromptRequest {
            kind: PromptKind::Confirm,
            title: title.into(),
            prompt: "p".into(),
            confirm_scope: scope,
            response_tx,
        })
        .unwrap();
        let mut q = SecretPromptQueue::new();
        q.poll_requests(&rx);
        q
    }

    #[test]
    fn allow_all_requires_an_attributed_tool_call_confirm() {
        let scope = ConfirmScope { extensions: vec!["jev".into()], tool: Some("bash".into()) };
        let title = crate::session::CONFIRM_PROMPT_TITLE;
        assert!(confirm(title, Some(scope.clone())).confirm_offers_allow_all());
        assert!(
            !confirm(title, None).confirm_offers_allow_all(),
            "an unattributed confirm cannot be latched, so no Allow all"
        );
        assert!(!confirm(
            title,
            Some(ConfirmScope { extensions: vec![], tool: None })
        )
        .confirm_offers_allow_all());
        assert!(!confirm(crate::session::CONFIRM_ACTIVATION_PROMPT_TITLE, Some(scope))
            .confirm_offers_allow_all());
        // Without a scope, AllowAll does nothing (no silent single Allow).
        let mut q = confirm(title, None);
        assert_eq!(q.confirm_answer(ConfirmChoice::AllowAll), None);
        assert!(q.is_active(), "prompt still waiting for a real answer");
    }

    #[test]
    fn focus_does_not_wrap_and_allow_all_needs_two_enters() {
        let scope = ConfirmScope { extensions: vec!["jev".into()], tool: None };
        let mut q = confirm(crate::session::CONFIRM_PROMPT_TITLE, Some(scope));
        q.arm_now_for_tests();
        q.move_confirm_focus(true);
        assert_eq!(q.confirm_focus(), ConfirmChoice::Deny, "no wrap past the end");
        q.move_confirm_focus(false);
        assert_eq!(q.confirm_focus(), ConfirmChoice::AllowAll);
        assert_eq!(q.confirm_activate_focused(), None, "first Enter only asks");
        assert!(q.allow_all_pending());
        assert!(!q.input_armed(), "the confirmation gets its own fresh guard");
        assert!(q.cancel_allow_all_pending());
        assert!(!q.allow_all_pending());
        assert_eq!(q.confirm_activate_focused(), None);
        q.arm_now_for_tests();
        assert_eq!(q.confirm_activate_focused(), Some(ConfirmChoice::AllowAll));
        assert!(!q.is_active());
    }

    #[test]
    fn dropping_allow_all_moves_focus_off_it() {
        let scope = ConfirmScope { extensions: vec!["jev".into()], tool: None };
        let mut q = confirm(crate::session::CONFIRM_PROMPT_TITLE, Some(scope));
        q.move_confirm_focus(false);
        assert_eq!(q.confirm_focus(), ConfirmChoice::AllowAll);
        q.drop_allow_all();
        assert!(!q.confirm_offers_allow_all());
        assert_eq!(q.confirm_focus(), ConfirmChoice::Deny);
        q.move_confirm_focus(false);
        assert_eq!(q.confirm_focus(), ConfirmChoice::Allow, "two buttons now");
    }

    #[test]
    fn confirm_ignores_input_until_armed_secret_never_waits() {
        let scope = ConfirmScope { extensions: vec!["jev".into()], tool: None };
        let mut q = confirm(crate::session::CONFIRM_PROMPT_TITLE, Some(scope));
        assert!(!q.input_armed(), "a fresh confirm is not armed");
        q.arm_now_for_tests();
        assert!(q.input_armed());
        assert!(SecretPromptQueue::new().input_armed(), "no prompt → nothing to guard");
    }
}
