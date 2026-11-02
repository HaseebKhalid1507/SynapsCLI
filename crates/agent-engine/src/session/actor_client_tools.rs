//! Client tools v1 in the session actor (contract §3, §6, §10;
//! docs/client-tools.md): the `register_client_tools` and `tool_return`
//! commands, the frames the overlay's calls send, and the lifecycle drops.
//! The overlay is `runtime/client_tools.rs`; the wire shapes and the field
//! checks are `session/client_tools.rs`. Every event here is addressed to
//! one client and never enters the replay ring (`SessionActor::emit`).
//!
//! The registration lives in the actor's overlay (RAM only), never in the
//! runtime, so a park keeps it and the unparked runtime is handed it again.
//! `tool_return` never touches the runtime: it answers a parked session too.

use serde_json::{Map, Value};

use super::actor::SessionActor;
use super::client_tools::{
    self as ct, ClientToolsAck, Evidence, Merged, Refusal, Refused, ToolCancelFrame, ToolInvokeFrame,
    ToolReturnAckFrame,
};
use super::types::*;
use crate::runtime::client_tools::{runtime_name, wire_name, DropReason, Outbound};
use crate::tools::ToolOrigin;

fn refusal(refusal: Refusal, message: impl Into<String>) -> Refused {
    Refused { refusal, message: message.into() }
}

impl SessionActor {
    /// What every `client_tools` event tells `client` about this session.
    fn client_tool_evidence(&self, client: ClientId) -> Evidence {
        let (conversation_id, forked_from) = if self.is_parked() {
            ((**self.journal_id.load()).clone(), self.fork.parked_from.clone())
        } else {
            (self.conv.session.id.clone(), self.conv.session.forked_from.clone())
        };
        Evidence {
            client: Some(client),
            conversation_id: Some(conversation_id),
            forked_from,
            pending: self.client_tools.pending(),
        }
    }

    /// A `client_tools` event for `client`; `echo`: the registration in
    /// force rides along (a `not_owner` reply and a drop notice echo none).
    fn client_tools_event(
        &self,
        client: ClientId,
        id: Option<u64>,
        outcome: &str,
        code: Option<(&str, String)>,
        echo: bool,
    ) -> SessionEventWire {
        let registration = if echo { self.client_tools.registration() } else { None };
        let policy = (!self.is_parked()).then(|| &self.runtime.memory_only_policy);
        let ack = ClientToolsAck::echo(
            id,
            outcome,
            code,
            registration.as_ref(),
            self.client_tool_evidence(client),
            |extension_id, runtime, wire| {
                policy.is_some_and(|policy| {
                    policy.marks(
                        &ToolOrigin::Extension { extension_id: extension_id.to_owned() },
                        runtime,
                        wire,
                    )
                })
            },
        );
        SessionEventWire::ClientTools { client: Some(client), ack: Box::new(ack) }
    }

    /// `register_client_tools` (contract §3). Only the input owner; the
    /// reply goes to the sender alone. A host-sent command gets no reply.
    pub(crate) async fn register_client_tools(
        &mut self,
        from: Option<ClientId>,
        id: u64,
        fields: Map<String, Value>,
    ) {
        let Some(client) = from else {
            return;
        };
        if self.input_owner != Some(client) {
            let code = (
                Refusal::NotOwner.code(),
                "only the session's input owner registers client tools".to_string(),
            );
            let event = self.client_tools_event(client, Some(id), "refused", Some(code), false);
            return self.emit(event);
        }
        let event = match self.try_register_client_tools(&fields).await {
            Ok(outcome) => {
                self.client_tool_installer = Some(client);
                self.client_tools_event(client, Some(id), outcome, None, true)
            }
            Err(refused) => self.client_tools_event(
                client,
                Some(id),
                "refused",
                Some((refused.refusal.code(), refused.message)),
                true,
            ),
        };
        self.emit(event);
    }

    /// The checks after the owner check, in order; a refusal changes
    /// nothing (R1: `def_changed` is decided before the install).
    async fn try_register_client_tools(
        &mut self,
        fields: &Map<String, Value>,
    ) -> Result<&'static str, Refused> {
        if !crate::config::load_config().client_tools {
            return Err(refusal(
                Refusal::FeatureOff,
                "client tools are off in this daemon's config (client_tools = off)",
            ));
        }
        if self.reload_fence.is_some() {
            return Err(refusal(Refusal::Busy, "the session is reloading; register again once it is back"));
        }
        ct::check_contract(fields)?;
        ct::check_generation(fields)?;
        ct::check_root(
            self.runtime.session_id(),
            self.conv.session.forked_from.is_some(),
            self.runtime.delegation_parent(),
        )?;
        let parsed = ct::parse_register(fields)?;
        let extension_id = parsed.extension_id.clone();
        let loaded = match self.host.ext_manager().try_read() {
            Ok(manager) => manager.list().contains(&extension_id.as_str()) || manager.is_deferred(&extension_id),
            Err(_) => return Err(refusal(Refusal::Busy, "extensions are loading; register again shortly")),
        };
        {
            let registry = self.runtime.tools_shared();
            let registry = registry.read().await;
            if loaded || !registry.tool_names_for_extension(&extension_id).is_empty() {
                return Err(refusal(Refusal::NameCollision, "extension_id is a loaded extension's id"));
            }
            for (i, tool) in parsed.tools.iter().enumerate() {
                let runtime = runtime_name(&extension_id, &tool.name);
                let wire = wire_name(&extension_id, &tool.name);
                if registry.get(&runtime).is_some() || registry.get(&wire).is_some() {
                    return Err(refusal(
                        Refusal::NameCollision,
                        format!("tools[{i}] is named like a tool this session already has"),
                    ));
                }
            }
        }
        let current = self.client_tools.registration();
        let (outcome, applied) = match ct::merge(current.as_ref(), parsed)? {
            Merged::Unchanged => ("unchanged", None),
            Merged::Applied(registration) => ("applied", Some(registration)),
        };
        if let Some(registration) = applied.as_ref().or(current.as_ref()).filter(|r| r.require_memory_only) {
            let origin = ToolOrigin::Extension { extension_id: registration.extension_id.clone() };
            let policy = &self.runtime.memory_only_policy;
            let unmarked = registration.tools.iter().position(|tool| {
                !policy.marks(
                    &origin,
                    &runtime_name(&registration.extension_id, &tool.name),
                    &wire_name(&registration.extension_id, &tool.name),
                )
            });
            if let Some(i) = unmarked {
                return Err(refusal(
                    Refusal::NotMarked,
                    format!("tools[{i}] is not marked memory-only in this session; set memory_only_tools first"),
                ));
            }
        }
        if let Some(registration) = applied {
            self.client_tools.install(registration);
        }
        Ok(outcome)
    }

    /// `tool_return` (contract §6): invalid, not_installer, then the
    /// overlay's checks. The reply goes to the sender alone; a host-sent
    /// return gets none. Never touches the runtime.
    pub(crate) fn client_tool_return(&mut self, from: Option<ClientId>, id: u64, fields: Map<String, Value>) {
        let Some(client) = from else {
            return;
        };
        let parsed = ct::parse_return(&fields);
        drop(fields);
        let invocation_id = parsed.as_ref().map(|ret| ret.invocation_id.clone());
        let code = match parsed {
            None => Some("invalid"),
            Some(_)
                if self.client_tool_installer != Some(client)
                    || self.client_tools.registration_id().is_none() =>
            {
                Some("not_installer")
            }
            Some(ret) => self
                .client_tools
                .resolve(&ret.invocation_id, &ret.outcome, ret.content, ret.meta_bytes, ret.memory_only)
                .err()
                .map(|refused| refused.code()),
        };
        let ack = ToolReturnAckFrame {
            id,
            invocation_id,
            outcome: if code.is_some() { "refused" } else { "accepted" }.to_string(),
            code: code.map(str::to_string),
        };
        self.emit(SessionEventWire::ToolReturnAck { client: Some(client), ack });
    }

    /// A frame one of the overlay's calls sent: to the installer of the
    /// registration it belongs to, or nowhere. An invoke whose call already
    /// ended is not sent.
    pub(crate) fn on_client_tool_frame(&mut self, frame: Outbound) {
        let current = self.client_tools.registration_id();
        let Some(installer) = self.client_tool_installer.filter(|_| current.is_some()) else {
            return;
        };
        match frame {
            Outbound::Invoke(invoke) => {
                if current.as_ref() == Some(&invoke.registration_id)
                    && self.client_tools.is_pending(&invoke.invocation_id)
                {
                    let invoke = Box::new(ToolInvokeFrame::from(invoke));
                    self.emit(SessionEventWire::ToolInvoke { client: Some(installer), invoke });
                }
            }
            Outbound::Cancel(cancel) => {
                if current.as_ref() == Some(&cancel.registration_id) {
                    let cancel = ToolCancelFrame::from(cancel);
                    self.emit(SessionEventWire::ToolCancel { client: Some(installer), cancel });
                }
            }
        }
    }

    /// The overlay's frames already queued, sent now: a turn cancel's
    /// `tool_cancel` (sent by the dropped call) reaches its installer before
    /// the turn's `Idle`.
    pub(crate) fn flush_client_tool_frames(&mut self) {
        while let Ok(frame) = self.client_tool_frames.try_recv() {
            self.on_client_tool_frame(frame);
        }
    }

    /// Drop the registration (contract §10): every live call ends now. An
    /// installer still attached gets the `tool_cancel` frames, then (owner
    /// change, new conversation, reload) a `client_tools{dropped}` notice:
    /// after a reload that aborts or is refused, the app knows to register
    /// again. Returns how many live calls got a cancel (none for a detach).
    pub(crate) fn drop_client_tools(&mut self, reason: DropReason) -> usize {
        let dropped = self.client_tools.drop_registration(reason);
        let installer = self.client_tool_installer.take();
        let ended = dropped.cancels.len();
        if dropped.registration.is_none() {
            return ended;
        }
        let Some(installer) = installer.filter(|client| self.attached.contains_key(client)) else {
            return ended;
        };
        for cancel in dropped.cancels {
            let cancel = ToolCancelFrame::from(cancel);
            self.emit(SessionEventWire::ToolCancel { client: Some(installer), cancel });
        }
        let notice = match reason {
            DropReason::OwnerChange => ("owner_change", "the session's input owner changed"),
            DropReason::NewConversation => ("new_conversation", "a new conversation started"),
            DropReason::Reload => {
                ("reload", "the daemon is reloading; register again once the session is back")
            }
            DropReason::Detached => return ended,
        };
        let event = self.client_tools_event(installer, None, "dropped", Some((notice.0, notice.1.to_string())), false);
        self.emit(event);
        ended
    }

    /// The input owner becomes `to`: a registration another client
    /// installed is dropped.
    pub(crate) fn client_tools_owner_changed(&mut self, to: Option<ClientId>) {
        if self.client_tool_installer.is_some_and(|installer| Some(installer) != to) {
            self.drop_client_tools(DropReason::OwnerChange);
        }
    }

    /// A compaction moved the conversation to a new id: the registration is
    /// kept, and its installer hears the new `conversation_id` before any
    /// invoke that carries it (contract §10).
    pub(crate) fn client_tools_rolled_over(&mut self) {
        let Some(installer) = self.client_tool_installer.filter(|client| self.attached.contains_key(client)) else {
            return;
        };
        if self.client_tools.registration_id().is_none() {
            return;
        }
        let code = ("rollover", "the conversation continues under a new id; the registration is kept".to_string());
        let event = self.client_tools_event(installer, None, "unchanged", Some(code), true);
        self.emit(event);
    }
}
