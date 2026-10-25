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
type Fingerprint = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Scope {
    pub(crate) account: String,
    pub(crate) model: String,
    pub(crate) session: String,
}

/// A finalized native turn. Deliberately has no Debug or serialization support.
pub(crate) struct RecordedTurn {
    scope: Scope,
    context: Fingerprint,
    visible: Fingerprint,
    parts: Vec<Value>,
    /// Client identity maps to the provider identity, including its absence.
    call_ids: HashMap<String, Option<String>>,
    bytes: usize,
}

/// Call observe before converting every native event. Call abort on a transport
/// error or disconnect, and finish only after a clean stream/body completion.
pub(crate) struct Recorder {
    scope: Scope,
    context: Fingerprint,
    parts: Vec<Value>,
    call_ids: HashMap<String, Option<String>>,
    call_order: Vec<String>,
    parts_bytes: usize,
    terminal: bool,
    invalid: bool,
}

impl Recorder {
    pub(crate) fn new(scope: Scope, outgoing_contents: Vec<Value>) -> Self {
        let mut context = history_hasher();
        for content in &outgoing_contents {
            hash_content(&mut context, content);
        }
        Self {
            scope,
            context: context.finalize().into(),
            parts: Vec::new(),
            call_ids: HashMap::new(),
            call_order: Vec::new(),
            parts_bytes: 2,
            terminal: false,
            invalid: false,
        }
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
                    if !self.invalid {
                        match id {
                            Some(id) if !id.is_empty() => {
                                if self
                                    .call_ids
                                    .insert(id.to_owned(), native_id.map(str::to_owned))
                                    .is_some()
                                {
                                    self.abort();
                                } else {
                                    self.call_order.push(id.to_owned());
                                }
                            }
                            _ => self.abort(),
                        }
                    }
                }
                // Record the unmodified provider part before adding the client
                // identity. Oversized parts are rejected before cloning them.
                if !self.invalid {
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
        if let Some(last) = self
            .parts
            .last_mut()
            .filter(|last| mergeable_text(last, part))
        {
            let extra = serialized_size(&part["text"]) - 2;
            if self.parts_bytes.saturating_add(extra) > MAX_TURN_BYTES {
                self.abort();
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
                self.abort();
                return;
            }
            self.parts_bytes += size + usize::from(!self.parts.is_empty());
            self.parts.push(part.clone());
        }
        if self.parts_bytes > MAX_TURN_BYTES {
            self.abort();
        }
    }

    pub(crate) fn abort(&mut self) {
        self.invalid = true;
        self.parts.clear();
        self.call_ids.clear();
        self.call_order.clear();
    }

    pub(crate) fn finish(self) -> Option<RecordedTurn> {
        if self.invalid
            || !self.terminal
            || self.parts.is_empty()
            || self.scope.session.trim().is_empty()
        {
            return None;
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
            + 4 * std::mem::size_of::<((Fingerprint, Fingerprint), RecordedTurn)>()
            + 128;
        Some(RecordedTurn {
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
    turns: HashMap<(Fingerprint, Fingerprint), RecordedTurn>,
    last_used: Instant,
    bytes: usize,
    in_flight: usize,
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
        Self::with_limits(8 * 1024 * 1024, MAX_TURN_BYTES, Duration::from_secs(3600))
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

    pub(crate) fn record(&mut self, turn: RecordedTurn) -> bool {
        self.record_at(turn, Instant::now())
    }

    fn record_at(&mut self, turn: RecordedTurn, now: Instant) -> bool {
        self.prune(now);
        if turn.scope.session.trim().is_empty()
            || serialized_size(&turn.parts) > self.max_turn_bytes
        {
            return false;
        }
        let key = (turn.context, turn.visible);
        let conversation = self.conversations.get(&turn.scope);
        let previous = conversation
            .and_then(|conversation| conversation.turns.get(&key))
            .map_or(0, |turn| turn.bytes);
        let overhead = conversation.map_or_else(|| conversation_bytes(&turn.scope), |_| 0);
        let additional = turn.bytes.saturating_add(overhead).saturating_sub(previous);
        // Never evict part of the conversation being extended. If the complete
        // history cannot fit, refuse this completion and retain the old state.
        if !self.make_room(&turn.scope, additional) {
            return false;
        }
        let conversation = self
            .conversations
            .entry(turn.scope.clone())
            .or_insert_with(|| Conversation {
                turns: HashMap::new(),
                last_used: now,
                bytes: overhead,
                in_flight: 0,
            });
        conversation.bytes = conversation.bytes + turn.bytes - previous;
        self.bytes = self.bytes + overhead + turn.bytes - previous;
        conversation.turns.insert(key, turn);
        // A generation can outlast the idle TTL while pinned. Its successful
        // completion starts a fresh idle period; refused recordings never do.
        conversation.last_used = now;
        true
    }

    /// Pin a fully validated conversation until the upstream call ends. Other
    /// sessions cannot evict its prefix while its next turn is being produced.
    pub(crate) fn begin(&mut self, scope: &Scope, request: &mut Value) -> Result<(), String> {
        let now = Instant::now();
        self.restore_at(scope, request, now)?;
        if scope.session.trim().is_empty() {
            return Ok(());
        }
        if !self.conversations.contains_key(scope) {
            let bytes = conversation_bytes(scope);
            if !self.make_room(scope, bytes) {
                return Err(
                    "signed Gemini history cache has no capacity for this conversation".into(),
                );
            }
            self.conversations.insert(
                scope.clone(),
                Conversation {
                    turns: HashMap::new(),
                    last_used: now,
                    bytes,
                    in_flight: 0,
                },
            );
            self.bytes += bytes;
        }
        self.conversations.get_mut(scope).unwrap().in_flight += 1;
        Ok(())
    }

    pub(crate) fn release(&mut self, scope: &Scope) {
        if let Some(conversation) = self.conversations.get_mut(scope) {
            conversation.in_flight = conversation.in_flight.saturating_sub(1);
            if conversation.in_flight == 0 && conversation.turns.is_empty() {
                self.remove(scope);
            }
        }
    }

    /// Validate the complete history before replacing any parts or renewing
    /// its TTL. Missing text turns require native replay just like tool turns.
    #[cfg(test)]
    pub(crate) fn restore(&mut self, scope: &Scope, request: &mut Value) -> Result<(), String> {
        self.restore_at(scope, request, Instant::now())
    }

    fn restore_at(
        &mut self,
        scope: &Scope,
        request: &mut Value,
        now: Instant,
    ) -> Result<(), String> {
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
        for (index, content) in contents.iter().enumerate() {
            if content.get("role").and_then(Value::as_str) == Some("model") {
                let parts = content
                    .get("parts")
                    .and_then(Value::as_array)
                    .ok_or("expected model parts array for signed-history replay")?;
                let current_context: Fingerprint = context.clone().finalize().into();
                let visible = fingerprint(&model_visible_parts(parts));
                let hit = self
                    .conversations
                    .get(scope)
                    .and_then(|conversation| conversation.turns.get(&(current_context, visible)));
                if let Some(turn) = hit {
                    replacements.push((index, turn.parts.clone()));
                    native_ids.extend(
                        turn.call_ids
                            .iter()
                            .map(|(client, native)| (client.as_str(), native.as_deref())),
                    );
                } else {
                    let ids = parts.iter().filter_map(|part| {
                        part.pointer("/functionCall/id").and_then(Value::as_str)
                    });
                    if ids.into_iter().any(|id| {
                        self.conversations.values().any(|conversation| {
                            conversation
                                .turns
                                .values()
                                .any(|turn| turn.call_ids.contains_key(id))
                        })
                    }) {
                        return Err("signed Gemini tool history does not match its recorded credential, model, session or preceding context".into());
                    }
                    let tools = parts.iter().any(|part| part.get("functionCall").is_some());
                    if !calls_are_signed(parts) || (!tools && !parts.iter().any(has_signature)) {
                        let kind = if tools {
                            "signed Gemini tool history"
                        } else {
                            "signed Gemini history"
                        };
                        return Err(if scope.session.trim().is_empty() {
                            format!("{kind} requires a stable session identifier")
                        } else {
                            format!(
                                "{kind} is unavailable for this credential, model and session; start a new conversation after cache expiry or restart"
                            )
                        });
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
        if let Some(conversation) = self.conversations.get_mut(scope) {
            conversation.last_used = now;
        }
        Ok(())
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

    fn make_room(&mut self, scope: &Scope, additional: usize) -> bool {
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
            .filter(|(candidate, conversation)| *candidate != scope && conversation.in_flight == 0)
            .map(|(scope, conversation)| {
                (scope.clone(), conversation.last_used, conversation.bytes)
            })
            .collect();
        victims.sort_by_key(|(_, last_used, _)| *last_used);
        if victims.iter().map(|(_, _, bytes)| bytes).sum::<usize>() < required {
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

fn detached_signature(part: &Value) -> bool {
    has_signature(part) && part.get("functionCall").is_none() && part.get("text").is_none()
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
    let Value::Array(parts) = visible_parts(parts) else {
        unreachable!("visible parts are always an array");
    };
    let mut text = String::new();
    let mut other_parts = Vec::new();
    for mut part in parts {
        if let Some(delta) = part.get("text").and_then(Value::as_str) {
            text.push_str(delta);
            part.as_object_mut().unwrap().remove("text");
            if part.as_object().unwrap().is_empty() {
                continue;
            }
        }
        other_parts.push(part);
    }
    if !text.is_empty() {
        other_parts.insert(0, serde_json::json!({"text":text}));
    }
    Value::Array(other_parts)
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
