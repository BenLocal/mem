//! Qwen3-Reranker on the candle stack — the I2 spike recipe, verbatim.
//!
//! Qwen3-Reranker is NOT a classification-head cross-encoder: its
//! architecture is a plain `Qwen3ForCausalLM`, and the official scoring
//! scheme is generative discrimination — wrap the (query, document)
//! pair in a chat template whose system prompt allows only "yes"/"no",
//! run ONE forward (no decoding), read the last position's logits for
//! the `yes`/`no` tokens, and softmax them: `P(yes)` is the relevance
//! score. `candle_transformers::models::qwen3::ModelForCausalLM`
//! already narrows its output to the last position, which is exactly
//! this shape.
//!
//! Spike-measured CPU numbers (96-core box, ~120-token pairs):
//! ~650-800ms/pair f32, thread-count plateau (bandwidth-bound). q8_0
//! GGUF via `quantized_qwen3` measured 12× SLOWER (its QMatMul kernels
//! are decode-optimized; prefill takes the slow path) — do not "help"
//! by quantizing. Padded batching is impossible through the public
//! forward (no attention-mask entry point) — pairs run sequentially.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::qwen3::{Config, ModelForCausalLM};
use std::sync::Mutex;
use tokenizers::Tokenizer;

use super::{RerankError, RerankProvider};

/// Official Qwen3-Reranker chat template (mirrors embed_anything's
/// qwen3 reranker path at the pinned commit — see rust/src/reranker/).
const PREFIX: &str = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n";
const SUFFIX: &str = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
const INSTRUCT: &str = "Given a web search query, retrieve relevant passages that answer the query";

pub struct CandleQwen3Reranker {
    /// `forward` needs `&mut` (KV cache); the trait is `&self`, so the
    /// model sits behind a mutex. Contention is a non-issue: providers
    /// are constructed per batch and used from one worker task.
    model: Mutex<ModelForCausalLM>,
    tokenizer: Tokenizer,
    model_name: String,
    yes_id: u32,
    no_id: u32,
    device: Device,
}

impl CandleQwen3Reranker {
    /// Load config + tokenizer + f32 weights from `dir`. ~1.5s and
    /// ~2.4GB resident — construct inside `spawn_blocking`, score the
    /// batch, drop.
    pub fn load(dir: &Path) -> Result<Self, RerankError> {
        let internal = |e: String| RerankError::Internal(e);
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .map_err(|e| internal(format!("read config.json in {dir:?}: {e}")))?,
        )
        .map_err(|e| internal(format!("parse config.json: {e}")))?;
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| internal(format!("load tokenizer.json in {dir:?}: {e}")))?;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &device,
            )
            .map_err(|e| internal(format!("mmap model.safetensors in {dir:?}: {e}")))?
        };
        let model = ModelForCausalLM::new(&config, vb)
            .map_err(|e| internal(format!("build qwen3 model: {e}")))?;
        let yes_id = tokenizer
            .token_to_id("yes")
            .ok_or_else(|| internal("tokenizer has no 'yes' token".into()))?;
        let no_id = tokenizer
            .token_to_id("no")
            .ok_or_else(|| internal("tokenizer has no 'no' token".into()))?;
        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            model_name: format!("qwen3-reranker@{}", dir.display()),
            yes_id,
            no_id,
            device,
        })
    }
}

impl RerankProvider for CandleQwen3Reranker {
    fn model(&self) -> &str {
        &self.model_name
    }

    fn score_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<f32>, RerankError> {
        let internal = |e: String| RerankError::Internal(e);
        let mut model = self
            .model
            .lock()
            .map_err(|_| internal("reranker model mutex poisoned".into()))?;
        let mut out = Vec::with_capacity(pairs.len());
        for (query, document) in pairs {
            let text = format!(
                "{PREFIX}<Instruct>: {INSTRUCT}\n<Query>: {query}\n<Document>: {document}{SUFFIX}"
            );
            let enc = self
                .tokenizer
                .encode(text, false)
                .map_err(|e| internal(format!("tokenize: {e}")))?;
            let input = Tensor::new(enc.get_ids(), &self.device)
                .and_then(|t| t.unsqueeze(0))
                .map_err(|e| internal(format!("input tensor: {e}")))?;
            let logits = model
                .forward(&input, 0)
                .map_err(|e| internal(format!("forward: {e}")))?;
            model.clear_kv_cache();
            let logits = logits
                .squeeze(0)
                .and_then(|t| t.squeeze(0))
                .map_err(|e| internal(format!("squeeze logits: {e}")))?;
            let yes = logits
                .get(self.yes_id as usize)
                .and_then(|t| t.to_scalar::<f32>())
                .map_err(|e| internal(format!("yes logit: {e}")))?;
            let no = logits
                .get(self.no_id as usize)
                .and_then(|t| t.to_scalar::<f32>())
                .map_err(|e| internal(format!("no logit: {e}")))?;
            out.push(yes.exp() / (yes.exp() + no.exp()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rerank::model_dir;

    /// Real-model smoke — needs the 1.19GB weights pre-warmed locally
    /// (not in the repo / CI), hence `#[ignore]`. Reproduces the spike's
    /// discrimination check: relevant ≫ irrelevant.
    #[test]
    #[ignore]
    fn real_model_discriminates_relevant_from_irrelevant() {
        let p = CandleQwen3Reranker::load(&model_dir()).expect("weights pre-warmed");
        let scores = p
            .score_pairs(&[
                (
                    "mem 的嵌入队列孤儿任务怎么自愈".into(),
                    "嵌入任务队列引入可见性租约：processing 孤儿超过 5 分钟自动重新入队，worker 崩溃后自愈。".into(),
                ),
                (
                    "mem 的嵌入队列孤儿任务怎么自愈".into(),
                    "导播台 multiview switcher 页面新增布局选择器和信号源面板。".into(),
                ),
            ])
            .unwrap();
        assert!(scores[0] > 0.9, "relevant pair: {scores:?}");
        assert!(scores[1] < 0.1, "irrelevant pair: {scores:?}");
    }
}
