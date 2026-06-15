//! Pooling strategies for embedding models.
//!
//! Different transformer models use different pooling strategies
//! to produce a fixed-size sentence embedding from variable-length
//! token-level outputs.

use crate::registry::PoolingStrategy;

/// Apply pooling strategy to token-level embeddings.
///
/// # Arguments
/// * `hidden_state` - Output tensor from ONNX model [seq_len, hidden_dim] (single batch)
/// * `attention_mask` - Attention mask [seq_len] where 1 = real token, 0 = padding
/// * `strategy` - Which pooling strategy to apply
///
/// # Returns
/// Pooled embedding vector of size [hidden_dim]
pub fn apply_pooling(
    hidden_state: &[Vec<f32>], // [seq_len, hidden_dim]
    attention_mask: &[i64],    // [seq_len]
    strategy: &PoolingStrategy,
) -> Vec<f32> {
    match strategy {
        PoolingStrategy::Cls => cls_pooling(hidden_state),
        PoolingStrategy::Mean => mean_pooling(hidden_state, attention_mask),
        PoolingStrategy::LastToken => last_token_pooling(hidden_state, attention_mask),
    }
}

/// CLS pooling: take the first token's embedding.
/// Used by BGE models.
fn cls_pooling(hidden_state: &[Vec<f32>]) -> Vec<f32> {
    hidden_state
        .first()
        .expect("hidden_state must have at least one token")
        .clone()
}

/// Mean pooling: average all token embeddings weighted by attention_mask.
/// Used by sentence-transformers models like MiniLM.
fn mean_pooling(hidden_state: &[Vec<f32>], attention_mask: &[i64]) -> Vec<f32> {
    let dim = hidden_state.first().map(|v| v.len()).unwrap_or(0);
    if dim == 0 {
        return Vec::new();
    }

    let mut sum = vec![0.0f32; dim];
    let mut count = 0.0f32;

    for (token_emb, &mask) in hidden_state.iter().zip(attention_mask.iter()) {
        if mask == 1 {
            for (s, v) in sum.iter_mut().zip(token_emb.iter()) {
                *s += *v;
            }
            count += 1.0;
        }
    }

    if count == 0.0 {
        return vec![0.0f32; dim];
    }

    sum.iter().map(|v| v / count).collect()
}

/// Last token pooling: take the last non-padding token's embedding.
/// Used by causal language models.
fn last_token_pooling(hidden_state: &[Vec<f32>], attention_mask: &[i64]) -> Vec<f32> {
    let last_idx = attention_mask
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| **m == 1)
        .map(|(i, _)| i)
        .unwrap_or(0);

    hidden_state[last_idx].clone()
}

/// L2-normalize a vector in-place and return it.
pub fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hidden_state(seq_len: usize, dim: usize) -> Vec<Vec<f32>> {
        (0..seq_len).map(|i| vec![i as f32 + 0.1; dim]).collect()
    }

    #[test]
    fn test_cls_pooling() {
        let hidden = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
        let mask = vec![1, 1];
        let result = apply_pooling(&hidden, &mask, &PoolingStrategy::Cls);
        assert_eq!(result, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_mean_pooling() {
        let hidden = vec![vec![2.0, 4.0], vec![4.0, 8.0]];
        let mask = vec![1, 1];
        let result = apply_pooling(&hidden, &mask, &PoolingStrategy::Mean);
        assert_eq!(result, vec![3.0, 6.0]); // (2+4)/2, (4+8)/2
    }

    #[test]
    fn test_mean_pooling_with_padding() {
        let hidden = vec![vec![2.0, 4.0], vec![0.0, 0.0], vec![6.0, 12.0]];
        let mask = vec![1, 0, 1]; // middle token is padding
        let result = apply_pooling(&hidden, &mask, &PoolingStrategy::Mean);
        assert_eq!(result, vec![4.0, 8.0]); // (2+6)/2, (4+12)/2
    }

    #[test]
    fn test_last_token_pooling() {
        let hidden = vec![vec![1.0], vec![2.0], vec![3.0]];
        let mask = vec![1, 1, 1];
        let result = apply_pooling(&hidden, &mask, &PoolingStrategy::LastToken);
        assert_eq!(result, vec![3.0]);
    }

    #[test]
    fn test_last_token_pooling_with_padding() {
        let hidden = vec![vec![1.0], vec![2.0], vec![0.0], vec![0.0]];
        let mask = vec![1, 1, 0, 0];
        let result = apply_pooling(&hidden, &mask, &PoolingStrategy::LastToken);
        assert_eq!(result, vec![2.0]);
    }

    #[test]
    fn test_l2_normalize() {
        let mut vec = vec![3.0, 4.0];
        l2_normalize(&mut vec);
        assert!((vec[0] - 0.6).abs() < 1e-6);
        assert!((vec[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_zero_vector() {
        let mut vec = vec![0.0, 0.0];
        l2_normalize(&mut vec);
        assert_eq!(vec, vec![0.0, 0.0]); // no division by zero
    }
}
