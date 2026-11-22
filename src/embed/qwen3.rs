pub const MODEL_ID: &str = "Qwen/Qwen3-Embedding-0.6B";

#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub model_id: &'a str,
    pub dim: usize,
    pub device: &'a str,
    pub precision: &'a str,
    pub instruction: &'a str,
    pub max_len: usize,
}

impl<'a> Options<'a> {
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

    /// Rows x longest-row ceiling for ONE forward pass: bounds the attention score tensor
    /// (`rows * T^2`) by the worst case a single max_len document already costs, and keeps
    /// padding waste from eating the win.
    const BATCH_TOKEN_BUDGET: usize = 8192;

    fn candle(err: candle_core::Error, what: &str) -> Error {
        Error::Model(format!("{what}: {err}"))
    }

    fn device_from(name: &str) -> Result<Device> {
        match name {
            "cpu" => Ok(Device::Cpu),
            "metal" => Device::new_metal(0)
                .map_err(|e| Error::Config(format!("cannot open metal device 0: {e}"))),
            other => Err(Error::Config(format!(
                "invalid device {other:?} (want {})",
                crate::config::DEVICES.join("|")
            ))),
        }
    }

    fn dtype_from(name: &str) -> Result<DType> {
        match name {
            "bf16" => Ok(DType::BF16),
            "f16" => Ok(DType::F16),
            "f32" => Ok(DType::F32),
            other => Err(Error::Config(format!(
                "invalid precision {other:?} (want {})",
                crate::config::PRECISIONS.join("|")
            ))),
        }
    }

    struct ModelPrefixBackend {
        inner: MmapedSafetensors,
        strip: bool,
    }

    impl ModelPrefixBackend {
        fn new(inner: MmapedSafetensors) -> Self {
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

    pub struct Qwen3Embedder {
        model: qwen3::Model,
        tokenizer: Tokenizer,
        device: Device,
        model_id: String,
        instruction: String,
        dim: usize,
    }

    impl Qwen3Embedder {
        pub fn load(dir: &Path, opts: &Options<'_>) -> Result<Self> {
            let batch = BatchConfig {
                padding: PaddingSide::Right,
                max_len: opts.max_len,
            };
            batch.validate()?;
            let device = device_from(opts.device)?;
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
                    "weights missing: {} (run `ellmephant model fetch --force`)",
                    weights.display()
                )));
            }
            // SAFETY: written with fsync + atomic rename, sha256-verified, so it cannot change under us.
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

        /// The pad token a short row is filled with. Its VALUE is immaterial: rows are
        /// padded on the RIGHT, and a causal query never attends to a position after
        /// itself, so no real token ever reads a pad. That is why batching needs no
        /// attention mask — see `forward_batch`.
        fn pad_id(&self) -> u32 {
            self.tokenizer
                .get_padding()
                .map(|p| p.pad_id)
                .or_else(|| self.tokenizer.token_to_id("<|endoftext|>"))
                .unwrap_or(0)
        }

        /// Encode several texts in as few forward passes as the padding budget allows.
        ///
        /// ★ WHY RIGHT PADDING, AND WHY THAT IS THE WHOLE TRICK. The natural reading of
        /// "batch the encoder" is "left-pad and stack [B,T]", which then needs an attention
        /// mask so a short row's last token does not read its pads. This candle (0.11)
        /// cannot carry one: `qwen3::Model::forward` takes no mask at all, and on CPU it
        /// hard-selects candle-nn's fused causal flash kernel, which REFUSES `B>1` with
        /// anything but `Causal`/`None` (cpu_flash/mod.rs:85). Right padding makes the mask
        /// unnecessary instead of unavailable: position i attends to 0..=i, real tokens
        /// occupy 0..len-1 exactly as they do unbatched, and pads sit at len..T where no
        /// causal query reads them. So the batched vector for a row is the SAME computation
        /// as `forward_one`, not an approximation of it. The only per-row difference is
        /// which position is pooled: `len-1`, not `T-1`.
        fn forward_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut tokenized: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
            for text in texts {
                let ids = tokenize::encode(&self.tokenizer, text)?;
                if ids.is_empty() {
                    return Err(Error::Config("empty tokenization".into()));
                }
                tokenized.push(ids);
            }

            let mut out: Vec<Vec<f32>> = Vec::with_capacity(tokenized.len());
            let mut start = 0;
            while start < tokenized.len() {
                // Grow the group while `rows * longest_row` stays inside the budget, so one
                // long document cannot drag its neighbours' padding into a 2x tensor, and a
                // batch of long rows degrades into the passes it would have made alone.
                let mut end = start + 1;
                let mut longest = tokenized[start].len();
                while end < tokenized.len() {
                    let wider = longest.max(tokenized[end].len());
                    if (end - start + 1) * wider > BATCH_TOKEN_BUDGET {
                        break;
                    }
                    longest = wider;
                    end += 1;
                }
                out.extend(self.forward_group(&tokenized[start..end], longest)?);
                start = end;
            }
            Ok(out)
        }

        /// One `[B,T]` forward, pooled at each row's own last real token.
        fn forward_group(&self, group: &[Vec<u32>], longest: usize) -> Result<Vec<Vec<f32>>> {
            let rows = group.len();
            let pad = self.pad_id();
            let mut flat: Vec<u32> = Vec::with_capacity(rows * longest);
            for ids in group {
                flat.extend(ids.iter().copied());
                flat.extend(std::iter::repeat_n(pad, longest - ids.len()));
            }
            let input = Tensor::from_vec(flat, (rows, longest), &self.device)
                .map_err(|e| candle(e, "batch input tensor"))?;

            let mut model = self.model.clone();
            let hidden = model
                .forward(&input, 0)
                .map_err(|e| candle(e, "qwen3 batch forward"))?;

            let mut out: Vec<Vec<f32>> = Vec::with_capacity(rows);
            for (row, ids) in group.iter().enumerate() {
                let pooled = hidden
                    .narrow(0, row, 1)
                    .and_then(|t| t.narrow(1, ids.len() - 1, 1))
                    .and_then(|t| t.flatten_all())
                    .and_then(|t| t.to_dtype(DType::F32))
                    .map_err(|e| candle(e, "pool last token"))?;
                let mut vector: Vec<f32> =
                    pooled.to_vec1().map_err(|e| candle(e, "read vector"))?;
                if vector.len() != self.dim {
                    return Err(Error::Model(format!(
                        "model produced dim {} but {} is configured",
                        vector.len(),
                        self.dim
                    )));
                }
                l2_normalize(&mut vector);
                out.push(vector);
            }
            Ok(out)
        }

        fn forward_one(&self, text: &str) -> Result<Vec<f32>> {
            let ids = tokenize::encode(&self.tokenizer, text)?;
            let len = ids.len();
            if len == 0 {
                return Err(Error::Config("empty tokenization".into()));
            }
            let input = Tensor::from_vec(ids, (1, len), &self.device)
                .map_err(|e| candle(e, "input tensor"))?;

            let mut model = self.model.clone();
            let hidden = model
                .forward(&input, 0)
                .map_err(|e| candle(e, "qwen3 forward"))?;

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
            self.forward_batch(texts)
        }

        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
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

    pub struct Qwen3Embedder {
        dim: usize,
    }

    impl Qwen3Embedder {
        pub fn load(_dir: &Path, opts: &Options<'_>) -> Result<Self> {
            let _ = opts;
            Err(Error::Unsupported(
                "built without the `encoder` feature (rebuild with `--features encoder`)",
            ))
        }

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
            Err(Error::Unsupported(
                "built without the `encoder` feature (rebuild with `--features encoder`)",
            ))
        }
    }
}
