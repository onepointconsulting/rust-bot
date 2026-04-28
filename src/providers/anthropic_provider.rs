use std::collections::HashMap;

use rand::seq::IndexedRandom;

use crate::providers::base::LLMProvider;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn gen_tool_id() -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..22)
        .map(|_| *ALNUM.choose(&mut rng).unwrap() as char)
        .collect();
    format!("toolu_{suffix}")
}

pub struct AnthropicProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: Option<HashMap<String, String>>,
}

impl AnthropicProvider {
}

// impl LLMProvider for AnthropicProvider {
// }

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_gen_tool_id() {
        let tool_id = gen_tool_id();
        assert!(tool_id.starts_with("toolu_"));
        assert_eq!(tool_id.len(), 28); // "toolu_" (6) + 22 alnum chars
        let suffix = &tool_id["toolu_".len()..];
        assert!(suffix.chars().all(|c| ALNUM.contains(&(c as u8))));
        println!("tool_id: {}", tool_id);
    }
}