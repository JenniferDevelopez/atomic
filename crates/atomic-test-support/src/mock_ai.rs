//! Wiremock-backed mock of the OpenAI-compat `/v1/embeddings` and
//! `/v1/chat/completions` endpoints.
//!
//! The provider in `atomic-core/src/providers/openai_compat.rs` is the real
//! reqwest client — `MockAiServer::start` just stands up an HTTP listener
//! that speaks the protocol it expects. Tests configure `AtomicCore` to
//! point at `base_url()`, then exercise the full pipeline (chunk → embed →
//! tag → edges) against deterministic responses.
//!
//! ## Mock responder modes
//!
//! [`ChatResponder`] currently emits **tag extraction** results keyed off
//! the request's `response_format.json_schema.name`. Slice 3 will extend
//! this with wiki-article and chat-tool-call modes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Embedding dimension used by the mock. Must stay in lockstep with the
/// default `openai_compat_embedding_dimension` setting and the SQLite
/// `vec_chunks float[1536]` schema so no dimension reconciliation kicks
/// in mid-test.
pub const EMBED_DIM: usize = 1536;

/// Similarity threshold used by the pipeline when building semantic edges.
/// Exposed here so tests can sanity-check that crafted atom pairs fall on
/// the correct side of the cutoff (see
/// `atomic_core::embedding::compute_semantic_edges...`).
pub const EDGE_SIMILARITY_THRESHOLD: f32 = 0.5;

/// Local HTTP server mimicking OpenAI's `/v1/embeddings` and
/// `/v1/chat/completions`. Holds the server handle for lifetime management.
pub struct MockAiServer {
    server: MockServer,
    counters: Arc<MockAiCounters>,
}

#[derive(Default)]
struct MockAiCounters {
    embedding_requests: AtomicUsize,
    chat_requests: AtomicUsize,
}

impl MockAiServer {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let counters = Arc::new(MockAiCounters::default());

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(EmbedResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        // Tag extraction goes through the non-streaming `complete` path
        // with a `response_format: json_schema` payload. The responder
        // inspects the request body so the same mock can serve any
        // structured call — for tagging we return a deterministic
        // `{"tags":[...]}` shape.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ChatResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        Self { server, counters }
    }

    /// Base URL the `OpenAICompatProvider` should hit. No `/v1` suffix —
    /// the provider normalizes the URL itself.
    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    pub fn embedding_request_count(&self) -> usize {
        self.counters.embedding_requests.load(Ordering::Relaxed)
    }

    pub fn chat_request_count(&self) -> usize {
        self.counters.chat_requests.load(Ordering::Relaxed)
    }

    pub fn reset_counts(&self) {
        self.counters.embedding_requests.store(0, Ordering::Relaxed);
        self.counters.chat_requests.store(0, Ordering::Relaxed);
    }
}

/// Bag-of-words style unit-vector embedder. Two texts sharing words land
/// at the same positions → high cosine similarity → edge crosses the 0.5
/// threshold. Disjoint texts end up near-orthogonal.
fn embed_text(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBED_DIM];
    for word in text.split_whitespace() {
        let normalized: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if normalized.is_empty() {
            continue;
        }
        let mut h = DefaultHasher::new();
        normalized.hash(&mut h);
        let idx = (h.finish() as usize) % EMBED_DIM;
        vec[idx] += 1.0;
    }
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    } else {
        // Empty/punctuation-only input — put a constant at position 0 so
        // every row still has a valid unit vector.
        vec[0] = 1.0;
    }
    vec
}

struct EmbedResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for EmbedResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters
            .embedding_requests
            .fetch_add(1, Ordering::Relaxed);
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };
        let Some(inputs) = body.get("input").and_then(|v| v.as_array()) else {
            return ResponseTemplate::new(400);
        };
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let text = text.as_str().unwrap_or_default();
                json!({
                    "object": "embedding",
                    "index": index,
                    "embedding": embed_text(text),
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": data,
            "model": body.get("model").cloned().unwrap_or(Value::Null),
        }))
    }
}

struct ChatResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for ChatResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters.chat_requests.fetch_add(1, Ordering::Relaxed);
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };

        // Inspect the requested schema name so this responder can serve
        // more than just tag extraction as the test matrix grows.
        let schema_name = body
            .pointer("/response_format/json_schema/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let request_text = body.to_string().to_lowercase();

        let content = match schema_name {
            "extraction_result" => {
                let tag_name = if request_text.contains("biology") {
                    "Biology"
                } else if request_text.contains("cooking") || request_text.contains("pasta") {
                    "Cooking"
                } else {
                    "Physics"
                };
                json!({
                    "tags": [
                        { "name": tag_name, "parent_name": "Topics" },
                    ]
                })
                .to_string()
            }
            // Default: empty content, still valid JSON for callers that
            // tolerate-parse. Individual tests can assert on the request
            // shape they care about.
            _ => "{}".to_string(),
        };

        ResponseTemplate::new(200).set_body_json(json!({
            "id": "mock-cmpl",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                    },
                    "finish_reason": "stop",
                }
            ],
        }))
    }
}
