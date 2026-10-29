//! Ephemeral native Gemini history, scoped to a credential, model and session.
//! Signed and hidden model parts stay in memory and are never client extensions.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Write},
    time::{Duration, Instant},
};

const MAX_TURN_BYTES: usize = 256 * 1024;
/// Gemini's documented placeholder for a function call whose original thought
/// signature is gone (history from another model, or replay lost to eviction).
pub(crate) const SKIP_SIGNATURE_VALIDATION: &str = "skip_thought_signature_validator";
type Fingerprint = [u8; 32];
pub(crate) type TurnId = u128;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Scope {
    pub(crate) account: String,
    pub(crate) model: String,
    pub(crate) session: String,
}

/// A finalized native turn. Deliberately has no Debug or serialization support.
pub(crate) struct RecordedTurn {
    id: TurnId,
    parent: Option<TurnId>,
    depth: usize,
    scope: Scope,
    context: Fingerprint,
    visible: Fingerprint,
    parts: Vec<Value>,
    /// Client identity maps to the provider identity, including its absence.
    call_ids: HashMap<String, Option<String>>,
    bytes: usize,
}

/// Recording failures describe native termination separately from cache capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordingFailure {
    Transport,
    SizeLimit,
    InvalidIdentity,
    MissingCompletion,
    Empty,
    Truncated,
    ProviderRefusal(&'static str),
    MissingSession,
}

impl std::fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Transport => {
                "native Gemini history recording was interrupted by a transport or upstream error"
            }
            Self::SizeLimit => "signed Gemini history exceeds its native turn size limit",
            Self::InvalidIdentity => {
                "native Gemini output has an invalid or duplicate tool identity"
            }
            Self::MissingCompletion => "upstream response did not contain a Gemini completion",
            Self::Empty => "upstream Gemini completion contains no replayable output",
            Self::Truncated => {
                "upstream Gemini completion was truncated before producing replayable output"
            }
            Self::ProviderRefusal(message) => message,
            Self::MissingSession => "signed Gemini history requires a stable session identifier",
        })
    }
}

/// Call observe before converting every native event. Call abort on a transport
/// error or disconnect, and finish only after a clean stream/body completion.
pub(crate) struct Recorder {
    parent: Option<TurnId>,
    scope: Scope,
    context: Fingerprint,
    parts: Vec<Value>,
    call_ids: HashMap<String, Option<String>>,
    call_order: Vec<String>,
    parts_bytes: usize,
    terminal: bool,
    failure: Option<RecordingFailure>,
    truncated: bool,
}

impl Recorder {
    pub(crate) fn new(scope: Scope, outgoing_contents: Vec<Value>) -> Self {
        let mut context = history_hasher();
        for content in &outgoing_contents {
            hash_content(&mut context, content);
        }
        Self {
            parent: None,
            scope,
            context: context.finalize().into(),
            parts: Vec::new(),
            call_ids: HashMap::new(),
            call_order: Vec::new(),
            parts_bytes: 2,
            terminal: false,
            failure: None,
            truncated: false,
        }
    }

    pub(crate) fn with_parent(mut self, parent: Option<TurnId>) -> Self {
        self.parent = parent;
        self
    }

    pub(crate) fn observe(&mut self, event: &mut Value) {
        if let Some(events) = event.as_array_mut() {
            for event in events {
                self.observe(event);
            }
            return;
        }
        if event.get("error").is_some() {
            self.abort();
        }
        let body = if event.get("response").is_some() {
            &mut event["response"]
        } else {
            event
        };
        if body.get("error").is_some() {
            self.abort();
        }
        if let Some(message) = crate::translate::gemini_refusal(body) {
            self.reject(RecordingFailure::ProviderRefusal(message));
        }
        let Some(candidate) = body
            .get_mut("candidates")
            .and_then(Value::as_array_mut)
            .and_then(|candidates| candidates.first_mut())
        else {
            return;
        };
        let terminal = candidate
            .get("finishReason")
            .or_else(|| candidate.get("finish_reason"))
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty() && reason != "FINISH_REASON_UNSPECIFIED");
        self.truncated |= candidate
            .get("finishReason")
            .or_else(|| candidate.get("finish_reason"))
            .and_then(Value::as_str)
            == Some("MAX_TOKENS");
        if let Some(parts) = candidate
            .get_mut("content")
            .and_then(|content| content.get_mut("parts"))
            .and_then(Value::as_array_mut)
        {
            for part in parts {
                let mut generated = None;
                if let Some(call) = part.get("functionCall").and_then(Value::as_object) {
                    let native_id = call.get("id").and_then(Value::as_str);
                    let id = if !call.contains_key("id") {
                        generated = Some(format!("call_douane_{:032x}", rand::random::<u128>()));
                        generated.as_deref()
                    } else {
                        native_id
                    };
                    if self.failure.is_none() {
                        match id {
                            Some(id) if !id.is_empty() => {
                                if self
                                    .call_ids
                                    .insert(id.to_owned(), native_id.map(str::to_owned))
                                    .is_some()
                                {
                                    self.reject(RecordingFailure::InvalidIdentity);
                                } else {
                                    self.call_order.push(id.to_owned());
                                }
                            }
                            _ => self.reject(RecordingFailure::InvalidIdentity),
                        }
                    }
                }
                // Record the unmodified provider part before adding the client
                // identity. Oversized parts are rejected before cloning them.
                if self.failure.is_none() {
                    self.push(part);
                }
                // Clients still need usable IDs after an abandoned recording
                // or when no stable session was supplied.
                if let Some(id) = generated {
                    part["functionCall"]["id"] = Value::String(id);
                }
            }
        }
        self.terminal |= terminal;
    }

    fn push(&mut self, part: &Value) {
        // Empty SSE boundary deltas are not historical text blocks. Keep all
        // signatures, thought flags, auxiliary fields and whitespace intact.
        if part.as_object().is_some_and(|part| {
            part.len() == 1 && part.get("text").and_then(Value::as_str) == Some("")
        }) {
            return;
        }
        if let Some(last) = self.parts.last_mut()
            && let Some(key) = closing_signature(last, part)
        {
            // A streamed signature closes the preceding unsigned text block.
            // Its key/value are the only new bytes in the assembled object.
            let extra = serialized_size(&key) + serialized_size(&part[key]) + 2;
            if self.parts_bytes.saturating_add(extra) > MAX_TURN_BYTES {
                self.reject(RecordingFailure::SizeLimit);
                return;
            }
            last[key] = part[key].clone();
            self.parts_bytes += extra;
            return;
        }
        if let Some(last) = self
            .parts
            .last_mut()
            .filter(|last| mergeable_text(last, part))
        {
            let extra = serialized_size(&part["text"]) - 2;
            if self.parts_bytes.saturating_add(extra) > MAX_TURN_BYTES {
                self.reject(RecordingFailure::SizeLimit);
                return;
            }
            if let Value::String(text) = &mut last["text"] {
                text.push_str(part["text"].as_str().unwrap());
            }
            self.parts_bytes += extra;
        } else {
            let size = serialized_size(part);
            if self
                .parts_bytes
                .saturating_add(size + usize::from(!self.parts.is_empty()))
                > MAX_TURN_BYTES
            {
                self.reject(RecordingFailure::SizeLimit);
                return;
            }
            self.parts_bytes += size + usize::from(!self.parts.is_empty());
            self.parts.push(part.clone());
        }
        if self.parts_bytes > MAX_TURN_BYTES {
            self.reject(RecordingFailure::SizeLimit);
        }
    }

    pub(crate) fn abort(&mut self) {
        self.reject(RecordingFailure::Transport);
    }

    fn reject(&mut self, failure: RecordingFailure) {
        self.failure.get_or_insert(failure);
        self.parts.clear();
        self.call_ids.clear();
        self.call_order.clear();
    }

    pub(crate) fn validate_completion(&self) -> Result<(), RecordingFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        if !self.terminal {
            return Err(RecordingFailure::MissingCompletion);
        }
        if self.parts.is_empty() {
            return Err(if self.truncated {
                RecordingFailure::Truncated
            } else {
                RecordingFailure::Empty
            });
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<RecordedTurn, RecordingFailure> {
        self.validate_completion()?;
        if self.scope.session.trim().is_empty() {
            return Err(RecordingFailure::MissingSession);
        }
        // Match the client's IDs without retaining a second copy of native
        // reasoning. Only this temporary visible projection uses client IDs.
        let mut client_parts = self.parts.clone();
        for (part, id) in client_parts
            .iter_mut()
            .filter(|part| part.get("functionCall").is_some())
            .zip(&self.call_order)
        {
            part["functionCall"]["id"] = Value::String(id.clone());
        }
        let visible = fingerprint(&model_visible_parts(&client_parts));
        let bytes = self.parts.iter().map(value_memory).sum::<usize>()
            + self.parts.capacity() * std::mem::size_of::<Value>()
            + self.scope.account.capacity()
            + self.scope.model.capacity()
            + self.scope.session.capacity()
            + self
                .call_ids
                .iter()
                .map(|(id, native)| id.capacity() + native.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self.call_ids.capacity() * 2 * (std::mem::size_of::<(String, Option<String>)>() + 1)
            // Reserve conservative space for the index's key, bucket slack and
            // allocation metadata, including the first small HashMap allocation.
            + 4 * std::mem::size_of::<(TurnId, RecordedTurn)>()
            + 4 * std::mem::size_of::<((Fingerprint, Fingerprint), Vec<TurnId>)>()
            + 4 * std::mem::size_of::<(Fingerprint, Vec<TurnId>)>()
            + 8 * std::mem::size_of::<TurnId>()
            + 384;
        Ok(RecordedTurn {
            id: rand::random(),
            parent: self.parent,
            depth: 0,
            scope: self.scope,
            context: self.context,
            visible,
            parts: self.parts,
            call_ids: self.call_ids,
            bytes,
        })
    }
}

struct Conversation {
    turns: HashMap<TurnId, RecordedTurn>,
    contexts: HashMap<(Fingerprint, Fingerprint), Vec<TurnId>>,
    visible: HashMap<Fingerprint, Vec<TurnId>>,
    last_used: Instant,
    bytes: usize,
    in_flight: usize,
}

impl Conversation {
    fn candidate(
        &self,
        context: Fingerprint,
        visible: Fingerprint,
    ) -> Result<Option<&RecordedTurn>, String> {
        let ids = self
            .contexts
            .get(&(context, visible))
            .or_else(|| self.visible.get(&visible));
        match ids.map(Vec::as_slice) {
            None => Ok(None),
            Some([id]) => Ok(self.turns.get(id)),
            Some(_) => Err("signed Gemini history has ambiguous native turn identity; start a new conversation".into()),
        }
    }

    fn follows(&self, previous: TurnId, next: &RecordedTurn) -> bool {
        let Some(previous) = self.turns.get(&previous) else {
            return false;
        };
        if next.depth <= previous.depth {
            return false;
        }
        let mut cursor = next;
        while cursor.depth > previous.depth {
            let Some(parent) = cursor.parent.and_then(|id| self.turns.get(&id)) else {
                return false;
            };
            cursor = parent;
        }
        cursor.id == previous.id
    }
}

/// A native call admitted by `ReplayCache::begin`.
pub(crate) struct Started {
    pub(crate) parent: Option<TurnId>,
    /// Whether the call holds a pin that `release` must drop when it ends.
    pub(crate) pinned: bool,
}

pub(crate) struct ReplayCache {
    conversations: BTreeMap<Scope, Conversation>,
    bytes: usize,
    max_bytes: usize,
    max_turn_bytes: usize,
    ttl: Duration,
}

impl Default for ReplayCache {
    fn default() -> Self {
        Self::for_settings(&crate::settings::Settings::default())
    }
}

impl ReplayCache {
    pub(crate) fn with_limits(max_bytes: usize, max_turn_bytes: usize, ttl: Duration) -> Self {
        Self {
            conversations: BTreeMap::new(),
            bytes: 0,
            max_bytes,
            max_turn_bytes,
            ttl,
        }
    }

    pub(crate) fn for_settings(settings: &crate::settings::Settings) -> Self {
        Self::with_limits(
            settings.antigravity_replay_bytes(),
            MAX_TURN_BYTES,
            settings.antigravity_replay_idle(),
        )
    }

    /// Apply changed settings at once: a smaller budget evicts idle
    /// conversations now and pinned ones when their call ends.
    pub(crate) fn configure(&mut self, settings: &crate::settings::Settings) {
        self.max_bytes = settings.antigravity_replay_bytes();
        self.ttl = settings.antigravity_replay_idle();
        self.prune(Instant::now());
        self.make_room(None, 0);
    }

    pub(crate) fn record(&mut self, turn: RecordedTurn) -> bool {
        self.record_at(turn, Instant::now())
    }

    fn record_at(&mut self, mut turn: RecordedTurn, now: Instant) -> bool {
        self.prune(now);
        if turn.scope.session.trim().is_empty()
            || serialized_size(&turn.parts) > self.max_turn_bytes
        {
            return false;
        }
        let conversation = self.conversations.get(&turn.scope);
        if conversation.is_some_and(|conversation| conversation.turns.contains_key(&turn.id)) {
            return false;
        }
        if let Some(parent) = turn.parent {
            let Some(parent) =
                conversation.and_then(|conversation| conversation.turns.get(&parent))
            else {
                return false;
            };
            let Some(depth) = parent.depth.checked_add(1) else {
                return false;
            };
            turn.depth = depth;
        }
        let key = (turn.context, turn.visible);
        // Identical completions may be retried; keep their existing immutable
        // identity. Different native generations never replace a referenced node.
        let duplicate = conversation
            .and_then(|conversation| {
                conversation.contexts.get(&key).map(|ids| {
                    ids.iter().any(|id| {
                        let old = &conversation.turns[id];
                        old.parent == turn.parent
                            && old.parts == turn.parts
                            && old.call_ids == turn.call_ids
                    })
                })
            })
            .unwrap_or(false);
        if duplicate {
            self.conversations.get_mut(&turn.scope).unwrap().last_used = now;
            return true;
        }
        let overhead = conversation.map_or_else(|| conversation_bytes(&turn.scope), |_| 0);
        let additional = turn.bytes.saturating_add(overhead);
        if !self.make_room(Some(&turn.scope), additional) {
            return false;
        }
        let conversation = self
            .conversations
            .entry(turn.scope.clone())
            .or_insert_with(|| Conversation {
                turns: HashMap::new(),
                contexts: HashMap::new(),
                visible: HashMap::new(),
                last_used: now,
                bytes: overhead,
                in_flight: 0,
            });
        conversation.bytes += turn.bytes;
        self.bytes += overhead + turn.bytes;
        conversation.contexts.entry(key).or_default().push(turn.id);
        conversation
            .visible
            .entry(turn.visible)
            .or_default()
            .push(turn.id);
        conversation.turns.insert(turn.id, turn);
        conversation.last_used = now;
        true
    }

    /// Pin a fully validated conversation until the upstream call ends. Other
    /// sessions cannot evict its prefix while its next turn is being produced.
    /// A conversation with no room beside pinned ones runs unpinned and unrecorded.
    pub(crate) fn begin(&mut self, scope: &Scope, request: &mut Value) -> Result<Started, String> {
        let now = Instant::now();
        let parent = self.restore_at(scope, request, now)?;
        if scope.session.trim().is_empty() {
            return Ok(Started {
                parent,
                pinned: false,
            });
        }
        if !self.conversations.contains_key(scope) {
            let bytes = conversation_bytes(scope);
            if !self.make_room(Some(scope), bytes) {
                return Ok(Started {
                    parent,
                    pinned: false,
                });
            }
            self.conversations.insert(
                scope.clone(),
                Conversation {
                    turns: HashMap::new(),
                    contexts: HashMap::new(),
                    visible: HashMap::new(),
                    last_used: now,
                    bytes,
                    in_flight: 0,
                },
            );
            self.bytes += bytes;
        }
        self.conversations.get_mut(scope).unwrap().in_flight += 1;
        Ok(Started {
            parent,
            pinned: true,
        })
    }

    /// Drop a pin taken by `begin`. A budget lowered during the call is
    /// enforced here, once nothing holds the conversation.
    pub(crate) fn release(&mut self, scope: &Scope) {
        let Some(conversation) = self.conversations.get_mut(scope) else {
            return;
        };
        conversation.in_flight = conversation.in_flight.saturating_sub(1);
        if conversation.in_flight == 0 {
            if conversation.turns.is_empty() {
                self.remove(scope);
            }
            self.make_room(None, 0);
        }
    }

    /// Validate the complete history before replacing any parts or renewing
    /// its TTL. Turns missing from the cache replay as the client sent them.
    #[cfg(test)]
    pub(crate) fn restore(&mut self, scope: &Scope, request: &mut Value) -> Result<(), String> {
        self.restore_at(scope, request, Instant::now()).map(|_| ())
    }

    pub(crate) fn has_session(&self, model: &str, session: &str) -> bool {
        self.conversations.keys().any(|scope| {
            scope.model == model
                && scope
                    .session
                    .strip_prefix(session)
                    .is_some_and(|suffix| suffix.starts_with(':'))
        })
    }

    /// Locate the original account without changing the history or its idle
    /// lifetime. A matching first turn identifies candidates, not authorization:
    /// the relay still applies account access, quota and reserve policies.
    pub(crate) fn owner(
        &mut self,
        model: &str,
        session: &str,
        request: &Value,
    ) -> Result<Option<String>, String> {
        self.owner_at(model, session, request, Instant::now())
    }

    fn owner_at(
        &mut self,
        model: &str,
        session: &str,
        request: &Value,
        now: Instant,
    ) -> Result<Option<String>, String> {
        self.prune(now);
        let Some(contents) = request.get("contents").and_then(Value::as_array) else {
            return Err("expected native Gemini contents array for signed-history replay".into());
        };
        // The first model turn still in the cache names the owner. Older turns
        // may have been evicted or never recorded and replay unsigned.
        let mut context = history_hasher();
        let mut scopes = Vec::new();
        for content in contents {
            if content["role"] == "model" {
                let parts = content["parts"]
                    .as_array()
                    .ok_or("expected model parts array for signed-history replay")?;
                let key: (Fingerprint, Fingerprint) = (
                    context.clone().finalize().into(),
                    fingerprint(&model_visible_parts(parts)),
                );
                for (scope, conversation) in &self.conversations {
                    if scope.model == model
                        && scope.session == session
                        && conversation.candidate(key.0, key.1)?.is_some()
                    {
                        scopes.push(scope.clone());
                    }
                }
                if !scopes.is_empty() {
                    break;
                }
            }
            hash_content(&mut context, content);
        }
        if scopes.len() > 1 {
            return Err(
                "signed Gemini history has ambiguous account ownership; start a new conversation"
                    .into(),
            );
        }
        let Some(scope) = scopes.into_iter().next() else {
            return Ok(None);
        };
        self.restore_with_touch(&scope, &mut request.clone(), now, false)?;
        Ok(Some(scope.account))
    }

    fn restore_at(
        &mut self,
        scope: &Scope,
        request: &mut Value,
        now: Instant,
    ) -> Result<Option<TurnId>, String> {
        self.restore_with_touch(scope, request, now, true)
    }

    fn restore_with_touch(
        &mut self,
        scope: &Scope,
        request: &mut Value,
        now: Instant,
        touch: bool,
    ) -> Result<Option<TurnId>, String> {
        self.prune(now);
        let path = if request.get("contents").is_some() {
            "/contents"
        } else {
            "/request/contents"
        };
        let contents = request
            .pointer(path)
            .and_then(Value::as_array)
            .ok_or("expected native Gemini contents array for signed-history replay")?;
        let mut context = history_hasher();
        let mut replacements = Vec::new();
        let mut native_ids = HashMap::new();
        let mut response_ids = Vec::new();
        let mut previous = None;
        let mut unsigned = Vec::new();
        for (index, content) in contents.iter().enumerate() {
            if content.get("role").and_then(Value::as_str) == Some("model") {
                let parts = content
                    .get("parts")
                    .and_then(Value::as_array)
                    .ok_or("expected model parts array for signed-history replay")?;
                let current_context: Fingerprint = context.clone().finalize().into();
                let visible = fingerprint(&model_visible_parts(parts));
                let conversation = self.conversations.get(scope);
                let hit = match conversation {
                    Some(conversation) => conversation.candidate(current_context, visible)?,
                    None => None,
                };
                if let Some(turn) = hit {
                    // Unrecorded turns may sit between recorded ones (eviction,
                    // a completion the cache could not keep), but the recorded
                    // turns themselves still form one ordered lineage.
                    if let Some(previous) = previous {
                        if !conversation.unwrap().follows(previous, turn) {
                            return Err(
                                "signed Gemini history does not follow one ordered native lineage"
                                    .into(),
                            );
                        }
                    }
                    previous = Some(turn.id);
                    replacements.push((index, turn.parts.clone()));
                    native_ids.extend(
                        turn.call_ids
                            .iter()
                            .map(|(client, native)| (client.as_str(), native.as_deref())),
                    );
                } else {
                    // An output recorded elsewhere never moves to this scope,
                    // not even as unsigned client text.
                    let ids = parts.iter().filter_map(|part| {
                        part.pointer("/functionCall/id").and_then(Value::as_str)
                    });
                    let mut elsewhere = self
                        .conversations
                        .iter()
                        .filter(|(other, _)| {
                            *other != scope
                                && other.model == scope.model
                                && other.session == scope.session
                        })
                        .map(|(_, conversation)| conversation.candidate(current_context, visible));
                    if ids.into_iter().any(|id| {
                        self.conversations.values().any(|conversation| {
                            conversation
                                .turns
                                .values()
                                .any(|turn| turn.call_ids.contains_key(id))
                        })
                    }) || elsewhere.any(|candidate| !matches!(candidate, Ok(None)))
                    {
                        return Err("signed Gemini tool history does not match its recorded credential, model, session or preceding context".into());
                    }
                    let tools = parts.iter().any(|part| part.get("functionCall").is_some());
                    let signed =
                        calls_are_signed(parts) && (tools || parts.iter().any(has_signature));
                    if !signed {
                        if scope.session.trim().is_empty() {
                            let kind = if tools {
                                "signed Gemini tool history"
                            } else {
                                "signed Gemini history"
                            };
                            return Err(format!("{kind} requires a stable session identifier"));
                        }
                        // Its native parts were evicted, expired or never
                        // recorded. Send the client's projection instead.
                        if !calls_are_signed(parts) {
                            unsigned.push(index);
                        }
                    }
                }
            }
            if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                for (part_index, part) in parts.iter().enumerate() {
                    if let Some(native) = part
                        .pointer("/functionResponse/id")
                        .and_then(Value::as_str)
                        .and_then(|id| native_ids.get(id))
                    {
                        response_ids.push((index, part_index, native.map(str::to_owned)));
                    }
                }
            }
            hash_content(&mut context, content);
        }
        let contents = request
            .pointer_mut(path)
            .and_then(Value::as_array_mut)
            .unwrap();
        for (index, parts) in replacements {
            contents[index]["parts"] = Value::Array(parts);
        }
        for index in unsigned {
            skip_signature_validation(&mut contents[index]["parts"]);
        }
        for (index, part_index, native) in response_ids {
            let response = contents[index]["parts"][part_index]["functionResponse"]
                .as_object_mut()
                .unwrap();
            if let Some(id) = native {
                response.insert("id".into(), Value::String(id));
            } else {
                response.remove("id");
            }
        }
        if touch {
            if let Some(conversation) = self.conversations.get_mut(scope) {
                conversation.last_used = now;
            }
        }
        Ok(previous)
    }

    fn prune(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .conversations
            .iter()
            .filter(|(_, conversation)| {
                conversation.in_flight == 0
                    && now.saturating_duration_since(conversation.last_used) >= self.ttl
            })
            .map(|(scope, _)| scope.clone())
            .collect();
        for scope in expired {
            self.remove(&scope);
        }
    }

    fn make_room(&mut self, scope: Option<&Scope>, additional: usize) -> bool {
        let required = self
            .bytes
            .saturating_add(additional)
            .saturating_sub(self.max_bytes);
        if required == 0 {
            return true;
        }
        let mut victims: Vec<_> = self
            .conversations
            .iter()
            .filter(|(candidate, conversation)| {
                Some(*candidate) != scope && conversation.in_flight == 0
            })
            .map(|(scope, conversation)| {
                (scope.clone(), conversation.last_used, conversation.bytes)
            })
            .collect();
        victims.sort_by_key(|(_, last_used, _)| *last_used);
        // A new turn either fits whole or evicts nothing; a reconfiguration
        // trims what it can and leaves pinned conversations in place.
        if scope.is_some() && victims.iter().map(|(_, _, bytes)| bytes).sum::<usize>() < required {
            return false;
        }
        let mut freed = 0;
        for (victim, _, bytes) in victims {
            self.remove(&victim);
            freed += bytes;
            if freed >= required {
                break;
            }
        }
        true
    }

    fn remove(&mut self, scope: &Scope) {
        if let Some(conversation) = self.conversations.remove(scope) {
            self.bytes -= conversation.bytes;
        }
    }
}

fn conversation_bytes(scope: &Scope) -> usize {
    std::mem::size_of::<Conversation>()
        + std::mem::size_of::<Scope>()
        + scope.account.len()
        + scope.model.len()
        + scope.session.len()
        + 256
}

fn has_signature(part: &Value) -> bool {
    ["thoughtSignature", "thought_signature"]
        .into_iter()
        .any(|key| {
            part.get(key)
                .and_then(Value::as_str)
                .is_some_and(|signature| !signature.is_empty())
        })
}

fn calls_are_signed(parts: &[Value]) -> bool {
    parts.iter().enumerate().all(|(index, part)| {
        part.get("functionCall").is_none()
            || has_signature(part)
            || index
                .checked_sub(1)
                .is_some_and(|previous| detached_signature(&parts[previous]))
            || parts.get(index + 1).is_some_and(detached_signature)
    })
}

/// Mark the first unsigned call of an unrecorded model step the way Gemini
/// documents for history whose signatures are gone. Later parallel calls of a
/// step carry no signature natively either, and text needs none.
fn skip_signature_validation(parts: &mut Value) {
    let Some(parts) = parts.as_array_mut() else {
        return;
    };
    if let Some(call) = parts
        .iter_mut()
        .find(|part| part.get("functionCall").is_some())
        .filter(|call| !has_signature(call))
    {
        call["thoughtSignature"] = Value::String(SKIP_SIGNATURE_VALIDATION.into());
    }
}

fn detached_signature(part: &Value) -> bool {
    has_signature(part) && part.get("functionCall").is_none() && part.get("text").is_none()
}

fn closing_signature(left: &Value, right: &Value) -> Option<&'static str> {
    let (left, right) = (left.as_object()?, right.as_object()?);
    if left.get("text").and_then(Value::as_str).is_none()
        || right.get("text").and_then(Value::as_str) != Some("")
        || [
            "thoughtSignature",
            "thought_signature",
            "functionCall",
            "functionResponse",
        ]
        .iter()
        .any(|key| left.contains_key(*key))
        || right.len() != left.len() + 1
        || !left
            .iter()
            .all(|(key, value)| key == "text" || right.get(key) == Some(value))
    {
        return None;
    }
    ["thoughtSignature", "thought_signature"]
        .into_iter()
        .find(|key| {
            right
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
}

fn mergeable_text(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    if left.get("text").and_then(Value::as_str).is_none()
        || right.get("text").and_then(Value::as_str).is_none()
        || [
            "thoughtSignature",
            "thought_signature",
            "functionCall",
            "functionResponse",
        ]
        .iter()
        .any(|key| left.contains_key(*key) || right.contains_key(*key))
    {
        return false;
    }
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| key == "text" || right.get(key) == Some(value))
}

fn visible_parts(parts: &[Value]) -> Value {
    let mut visible: Vec<Value> = Vec::new();
    for part in parts {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let mut part = part.clone();
        if let Some(object) = part.as_object_mut() {
            object.remove("thoughtSignature");
            object.remove("thought_signature");
            object.remove("thought");
            if object.is_empty() {
                continue;
            }
        }
        if let Some(last) = visible
            .last_mut()
            .filter(|last| mergeable_text(last, &part))
        {
            if let Value::String(text) = &mut last["text"] {
                text.push_str(part["text"].as_str().unwrap());
            }
        } else {
            visible.push(part);
        }
    }
    Value::Array(visible)
}

/// Chat-compatible clients store one text field and a separate ordered call
/// list. Match that projection while keeping the original native ordering in
/// RecordedTurn. User parts retain their order in the preceding-history hash.
fn model_visible_parts(parts: &[Value]) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in parts {
        // This is the translated wire projection, not a filtered native part.
        if part["thought"] != true {
            if let Some(delta) = part.get("text").and_then(Value::as_str) {
                text.push_str(delta);
            }
        }
        if let Some(call) = part.get("functionCall") {
            calls.push(serde_json::json!({"functionCall":{
                "id":call.get("id").cloned().unwrap_or(Value::Null),
                "name":call["name"],"args":call["args"]
            }}));
        }
        // Unsupported media must never become an invisible replay identity.
        for field in ["inlineData", "fileData"] {
            if let Some(value) = part.get(field) {
                calls.push(serde_json::json!({(field):value}));
            }
        }
    }
    if !text.is_empty() {
        calls.insert(0, serde_json::json!({"text":text}));
    }
    Value::Array(calls)
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical(&object[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}

fn history_hasher() -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(b"douane-gemini-native-history-v1\0");
    hasher
}

fn hash_content(hasher: &mut Sha256, content: &Value) {
    let mut content = content.clone();
    if let Some(parts) = content.get("parts").and_then(Value::as_array) {
        content["parts"] = if content.get("role").and_then(Value::as_str) == Some("model") {
            model_visible_parts(parts)
        } else {
            visible_parts(parts)
        };
    }
    let encoded = serde_json::to_vec(&canonical(&content)).expect("JSON values serialize");
    hasher.update((encoded.len() as u64).to_be_bytes());
    hasher.update(encoded);
}

fn fingerprint(value: &Value) -> Fingerprint {
    Sha256::digest(serde_json::to_vec(&canonical(value)).expect("JSON values serialize")).into()
}

struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_size(value: &impl serde::Serialize) -> usize {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).expect("JSON values serialize");
    counter.0
}

/// Conservatively account for heap allocations, including object/tree overhead.
fn value_memory(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(text) => text.capacity(),
            Value::Array(array) => {
                array.capacity() * std::mem::size_of::<Value>()
                    + array.iter().map(value_memory).sum::<usize>()
            }
            Value::Object(object) => object
                .iter()
                .map(|(key, value)| key.capacity() + 128 + value_memory(value))
                .sum::<usize>(),
            _ => 0,
        }
}

#[cfg(test)]
mod tests;
