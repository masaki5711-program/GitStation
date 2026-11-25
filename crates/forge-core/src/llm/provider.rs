//! The provider trait and the checked call (plan 19 §2).
//!
//! [`LlmProvider`] is one chat call to a local model (the Ollama provider
//! lives in `forge-llm`; tests use stand-ins). [`generate_checked`] sends
//! the prepared input, checks the answer ([`check_output`]) and, when the
//! answer is invalid, asks **once** more with the reason; a second invalid
//! answer is `FG-LLM-0002` (`attempts=2`). A provider failure is returned
//! as it is (the gateway decides about `FG-LLM-0001`): never a success.

use std::future::Future;
use std::pin::Pin;

use forge_errors::{ErrorCode, ForgeError};

use super::output::{check_output, Advice, OutputPolicy};
use super::PreparedInput;

/// One chat call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    /// `name:tag` (the digest is checked by the gateway, not per call).
    pub model: String,
    /// The instructions (the versioned prompt).
    pub system: String,
    /// The data ([`PreparedInput::render`]).
    pub user: String,
    /// The context window (the same as every other user of the shared
    /// Ollama, so the model is not reloaded: plan 19 §0.3 7).
    pub num_ctx: u32,
}

/// The model's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub content: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// The boxed future of [`LlmProvider::chat`].
pub type ChatFuture<'a> = Pin<Box<dyn Future<Output = Result<ChatReply, ForgeError>> + Send + 'a>>;

/// One local model behind an HTTP API (or a stand-in in tests). Every
/// implementation bounds the call with a timeout.
pub trait LlmProvider: Send + Sync {
    fn chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a>;
}

/// A checked advice and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    pub advice: Advice,
    /// 1, or 2 after one invalid answer.
    pub attempts: u8,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// The note added to the second attempt.
const RETRY_NOTE: &str = "\n\nYour previous answer was rejected: it did not match the required \
     JSON shape. Answer again with exactly one JSON object of the required shape and nothing else.";

/// Ask, check, and retry once on an invalid answer (module docs).
pub async fn generate_checked(
    provider: &dyn LlmProvider,
    request: &ChatRequest,
    input: &PreparedInput,
    policy: &OutputPolicy,
) -> Result<Generated, ForgeError> {
    let first = provider.chat(request).await?;
    let mut input_tokens = first.input_tokens;
    let mut output_tokens = first.output_tokens;
    let first_reason = match check_output(&first.content, input, policy) {
        Ok(advice) => {
            return Ok(Generated {
                advice,
                attempts: 1,
                input_tokens,
                output_tokens,
            })
        }
        Err(e) => reason_of(&e),
    };
    let mut again = request.clone(); // clone: the retry adds a note to its own copy
    again.system.push_str(RETRY_NOTE);
    let second = provider.chat(&again).await?;
    input_tokens = input_tokens.saturating_add(second.input_tokens);
    output_tokens = output_tokens.saturating_add(second.output_tokens);
    match check_output(&second.content, input, policy) {
        Ok(advice) => Ok(Generated {
            advice,
            attempts: 2,
            input_tokens,
            output_tokens,
        }),
        Err(e) => Err(ForgeError::new(ErrorCode::LlmOutputInvalid)
            .with_context("reason", reason_of(&e))
            .with_context("first_reason", first_reason)
            .with_context("attempts", 2)),
    }
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or_else(|| "unknown".to_owned(), |(_, v)| v.clone())
}
