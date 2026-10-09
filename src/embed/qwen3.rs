//! Qwen3-Embedding encoder on candle (P3).
//!
//! We own the encoder rather than depend on `fastembed` (which drags in ONNX Runtime and
//! mean-pools — PLAN §1 F1/F2). The weights come from the P2.5 cache as a flat dir; this
//! module only *loads* them.
//!
//! Contract (SPEC §6): 1024-d, **last-token** pooling, left padding, L2-normalised, with
//! `Instruct: <task>\nQuery: <q>` prepended on the *query* side only.
//!
//! Compiled only with the default-on `encoder` feature; `--no-default-features` leaves a
//! stub that errors at runtime, keeping the store/search layers buildable without candle.

/// HF repo id recorded in `meta` when the encoder is the default Qwen3 model.
pub const MODEL_ID: &str = "Qwen/Qwen3-Embedding-0.6B";

/// How to load the encoder. `model_id` is folded into the row hash, so a different repo
/// (or `--model-dir`) produces a different hash and forces a re-embed.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// Model id recorded in `meta` / the row hash.
    pub model_id: &'a str,
    /// Expected embedding length; must match the loaded model.
    pub dim: usize,
    /// `cpu` or `metal` (SPEC §15 #9).
    pub device: &'a str,
    /// `bf16` (default) or `f16`.
    pub precision: &'a str,
    /// Query-side instruction (SPEC §6); empty disables the prefix.
    pub instruction: &'a str,
    /// Truncation length.
    pub max_len: usize,
}

impl<'a> Options<'a> {
    /// Options for the default model, honouring `device`/`precision`/`instruction`.
    pub fn new(dim: usize, device: &'a str, precision: &'a str, instruction: &'a str) -> Self {
        Self {
            model_id: MODEL_ID,
            dim,
            device,
            precision,
            instruction,
            max_len: crate::embed::tokenize::BatchConfig::default().max_len,
        }
    }
}

#[cfg(feature = "encoder")]
pub use real::Qwen3Embedder;

#[cfg(not(feature = "encoder"))]
pub use stub::Qwen3Embedder;
#[cfg(feature = "encoder")]
mod real {
    use std::borrow::Cow;
    use std::path::Path;

    use candle_core::safetensors::MmapedSafetensors;
    use candle_core::{DType, Device, Shape, Tensor};
    use candle_nn::var_builder::SimpleBackend;
    use candle_nn::{Init, VarBuilder};
    use candle_transformers::models::qwen3;
    use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

    use super::Options;
    use crate::config::DEFAULT_INSTRUCTION;
    use crate::embed::Embedder;
    use crate::embed::l2_normalize;
    use crate::embed::tokenize::{self, BatchConfig, PaddingSide};
    use crate::error::{Error, Result};

    /// Map a candle failure into our error type, naming the step that failed.
    fn candle(err: candle_core::Error, what: &str) -> Error {
        Error::Model(format!("{what}: {err}"))
    }

    /// Parse `cpu`/`metal` (SPEC §15 #9).
    fn device_from(name: &str) -> Result<Device> {
        match name {
            "cpu" => Ok(Device::Cpu),
            "metal" => Device::new_metal(0)
                .map_err(|e| Error::Config(format!("cannot open metal device 0: {e}"))),
            other => Err(Error::Config(format!(
                "invalid device {other:?} (want cpu|metal)"
            ))),
        }
    }

    /// Parse `bf16`/`f16`/`f32` (SPEC §6).
    fn dtype_from(name: &str) -> Result<DType> {
        match name {
            "bf16" => Ok(DType::BF16),
            "f16" => Ok(DType::F16),
            "f32" => Ok(DType::F32),
            other => Err(Error::Config(format!(
                "invalid precision {other:?} (want bf16|f16|f32)"
            ))),
        }
    }

    /// Bridge between the HF file's naming and candle's.
    ///
    /// `Qwen/Qwen3-Embedding-0.6B` stores the transformer under *bare* names
    /// (`embed_tokens.weight`, `layers.0.…`), while `qwen3::Model` asks for the same tensors
    /// under a `model.` prefix. This adapter strips that prefix on lookup, so we load the
    /// shipped file directly instead of rewriting 1.2 GB.
    struct ModelPrefixBackend {
        inner: MmapedSafetensors,
        strip: bool,
    }

    impl ModelPrefixBackend {
        fn new(inner: MmapedSafetensors) -> Self {
            // Only strip when the file does *not* already use the prefixed names.
            let strip = !inner
                .tensors()
                .iter()
                .any(|(name, _)| name == "model.embed_tokens.weight");
            Self { inner, strip }
        }

        fn remap<'a>(&self, name: &'a str) -> Cow<'a, str> {
            match (self.strip, name.strip_prefix("model.")) {
                (true, Some(rest)) => Cow::Borrowed(rest),
                _ => Cow::Borrowed(name),
            }
        }
    }

    impl SimpleBackend for ModelPrefixBackend {
        fn get(
            &self,
            s: Shape,
            name: &str,
            h: Init,
            dtype: DType,
            dev: &Device,
        ) -> candle_core::Result<Tensor> {
            SimpleBackend::get(&self.inner, s, &self.remap(name), h, dtype, dev)
        }

        fn get_unchecked(
            &self,
            name: &str,
            dtype: DType,
            dev: &Device,
        ) -> candle_core::Result<Tensor> {
            SimpleBackend::get_unchecked(&self.inner, &self.remap(name), dtype, dev)
        }

        fn contains_tensor(&self, name: &str) -> bool {
            SimpleBackend::contains_tensor(&self.inner, &self.remap(name))
        }
    }

    /// The Qwen3-Embedding encoder, loaded from a flat weights directory.
    pub struct Qwen3Embedder {
        model: qwen3::Model,
        tokenizer: Tokenizer,
        device: Device,
        model_id: String,
        instruction: String,
        dim: usize,
    }

    impl Qwen3Embedder {
        /// Load `config.json`, `model.safetensors` and `tokenizer.json` from `dir`.
        ///
        /// `dir` is a cache revision directory (or a `--model-dir`); this function never
        /// touches the network — the cache layer (P2.5) guarantees the files are present.
        pub fn load(dir: &Path, opts: &Options<'_>) -> Result<Self> {
            let batch = BatchConfig {
                padding: PaddingSide::Left,
                max_len: opts.max_len,
            };
            batch.validate()?;
            let device = device_from(opts.device)?;
            // candle's CPU matmul goes through Accelerate, which supports f32 only — no
            // bf16, no f16 (PLAN §9 risk). CPU therefore runs f32; GPU honours `--precision`.
            let dtype = if device.is_cpu() {
                tracing::debug!("cpu backend: forcing f32 (accelerate has no bf16/f16 matmul)");
                DType::F32
            } else {
                dtype_from(opts.precision)?
            };

            let config_path = dir.join("config.json");
            let config_text = std::fs::read_to_string(&config_path)
                .map_err(|e| Error::Model(format!("read {}: {e}", config_path.display())))?;
            let config: qwen3::Config = serde_json::from_str(&config_text)
                .map_err(|e| Error::Model(format!("parse {}: {e}", config_path.display())))?;

            let weights = dir.join("model.safetensors");
            if !weights.is_file() {
                return Err(Error::Model(format!(
                    "weights missing: {} (run `llmphant model fetch --force`)",
                    weights.display()
                )));
            }
            // SAFETY: mmaps the weights file; the cache wrote it with fsync + atomic rename
            // and verified its sha256 (SPEC §6.2–6.3), so it cannot change under us.
            let mmap = unsafe { MmapedSafetensors::new(&weights) }
                .map_err(|e| candle(e, &format!("mmap {}", weights.display())))?;
            let vb = VarBuilder::from_backend(
                Box::new(ModelPrefixBackend::new(mmap)),
                dtype,
                device.clone(),
            );
            let model = qwen3::Model::new(&config, vb).map_err(|e| candle(e, "build qwen3"))?;

            let tokenizer_path = dir.join("tokenizer.json");
            let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| Error::Model(format!("load {}: {e}", tokenizer_path.display())))?;
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: opts.max_len,
                    strategy: TruncationStrategy::LongestFirst,
                    stride: 0,
                    direction: TruncationDirection::Right,
                }))
                .map_err(|e| Error::Model(format!("configure truncation: {e}")))?;

            let instruction = if opts.instruction.trim().is_empty() {
                DEFAULT_INSTRUCTION.to_string()
            } else {
                opts.instruction.trim().to_string()
            };

            Ok(Self {
                model,
                tokenizer,
                device,
                model_id: opts.model_id.to_string(),
                instruction,
                dim: opts.dim,
            })
        }

        /// Tokenise, run one forward, last-token pool and L2-normalise. One sequence at a
        /// time — see the module docs on why padded batching is unsafe with this forward.
        fn forward_one(&self, text: &str) -> Result<Vec<f32>> {
            let ids = tokenize::encode(&self.tokenizer, text)?;
            let len = ids.len();
            if len == 0 {
                return Err(Error::Config("empty tokenization".into()));
            }
            let input = Tensor::from_vec(ids, (1, len), &self.device)
                .map_err(|e| candle(e, "input tensor"))?;

            // `forward` mutates an internal KV cache; clone a pristine model so each call
            // starts at offset 0 with an empty cache (Tensor clones are cheap Arc bumps).
            let mut model = self.model.clone();
            let hidden = model
                .forward(&input, 0)
                .map_err(|e| candle(e, "qwen3 forward"))?; // (1, L, H)

            let pooled = hidden
                .narrow(1, len - 1, 1)
                .and_then(|t| t.flatten_all())
                .and_then(|t| t.to_dtype(DType::F32))
                .map_err(|e| candle(e, "pool last token"))?;
            let mut vector: Vec<f32> = pooled.to_vec1().map_err(|e| candle(e, "read vector"))?;

            if vector.len() != self.dim {
                return Err(Error::Model(format!(
                    "model produced dim {} but {} is configured",
                    vector.len(),
                    self.dim
                )));
            }
            l2_normalize(&mut vector);
            Ok(vector)
        }

        /// The instruction prefix actually applied to queries.
        pub fn instruction(&self) -> &str {
            &self.instruction
        }
    }

    impl Embedder for Qwen3Embedder {
        fn dim(&self) -> usize {
            self.dim
        }

        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            texts.iter().map(|t| self.forward_one(t)).collect()
        }

        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
            // An empty instruction means documents and queries are symmetric.
            let prefixed = if self.instruction.is_empty() {
                text.to_string()
            } else {
                format!("Instruct: {}\nQuery: {text}", self.instruction)
            };
            self.forward_one(&prefixed)
        }
    }
}

#[cfg(not(feature = "encoder"))]
mod stub {
    use std::path::Path;

    use super::Options;
    use crate::embed::Embedder;
    use crate::error::{Error, Result};

    /// Placeholder used when the crate is built without the `encoder` feature.
    pub struct Qwen3Embedder {
        dim: usize,
    }

    impl Qwen3Embedder {
        /// Always fails: this build has no candle backend.
        pub fn load(_dir: &Path, opts: &Options<'_>) -> Result<Self> {
            let _ = opts;
            Err(Error::NotImplemented(
                "built without the `encoder` feature (rebuild with `--features encoder`)",
            ))
        }

        /// No instruction is ever applied by the stub.
        pub fn instruction(&self) -> &str {
            ""
        }
    }

    impl Embedder for Qwen3Embedder {
        fn dim(&self) -> usize {
            self.dim
        }

        fn model_id(&self) -> &str {
            super::MODEL_ID
        }

        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Err(Error::NotImplemented(
                "built without the `encoder` feature (rebuild with `--features encoder`)",
            ))
        }
    }
}
