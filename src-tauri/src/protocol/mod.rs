pub mod anthropic;
pub mod codec;
mod detect;
mod legacy;
pub mod responses;
pub mod sse_bridge;
pub mod thinking;

pub use detect::{extract_api_key, is_anthropic_request, is_responses_request};
pub use legacy::{
    anthropic_to_openai, estimate_anthropic_input_tokens, openai_to_anthropic, openai_to_responses,
    responses_to_openai,
};
