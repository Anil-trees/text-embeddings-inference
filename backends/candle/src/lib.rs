mod alibi;
#[cfg(feature = "cuda")]
mod compute_cap;
#[cfg(feature = "cuda")]
mod flash_attn;
mod layers;
mod models;

#[cfg(feature = "cuda")]
use crate::compute_cap::{
    get_compile_compute_cap, get_runtime_compute_cap, incompatible_compute_cap,
};
#[cfg(feature = "cuda")]
use crate::models::FlashBertModel;
#[cfg(feature = "cuda")]
use crate::models::FlashJinaBertModel;
use crate::models::{BertModel, JinaBertModel, Model, PositionEmbeddingType};
use candle::{DType, Device};
use candle_nn::VarBuilder;
use models::Config;
use nohash_hasher::BuildNoHashHasher;
use std::collections::HashMap;
use std::path::PathBuf;
use text_embeddings_backend_core::{
    Backend, BackendError, Batch, Embedding, Embeddings, ModelType, Predictions,
};

pub struct CandleBackend {
    model: Box<dyn Model + Send>,
}

impl CandleBackend {
    pub fn new(
        model_path: PathBuf,
        dtype: String,
        model_type: ModelType,
    ) -> Result<Self, BackendError> {
        // Load config
        let config: String = std::fs::read_to_string(model_path.join("config.json"))
            .map_err(|err| BackendError::Start(err.to_string()))?;
        let config: Config =
            serde_json::from_str(&config).map_err(|err| BackendError::Start(err.to_string()))?;

        // Get candle device
        let device = if candle::utils::cuda_is_available() {
            Device::new_cuda(0)
        } else if candle::utils::metal_is_available() {
            Device::new_metal(0)
        } else {
            Ok(Device::Cpu)
        }
        .map_err(|err| BackendError::Start(err.to_string()))?;

        // Check model type
        if config.model_type != Some("bert".to_string())
            && config.model_type != Some("xlm-roberta".to_string())
            && config.model_type != Some("camembert".to_string())
            && config.model_type != Some("roberta".to_string())
        {
            return Err(BackendError::Start(format!(
                "Model {:?} is not supported",
                config.model_type
            )));
        }

        // Get candle dtype
        let dtype = if &dtype == "float32" {
            Ok(DType::F32)
        } else if &dtype == "float16" {
            Ok(DType::F16)
        } else {
            Err(BackendError::Start(format!(
                "DType {dtype} is not supported"
            )))
        }?;

        let safetensors_path = model_path.join("model.safetensors");
        let vb = if safetensors_path.exists() {
            unsafe {
                VarBuilder::from_mmaped_safetensors(
                    &[model_path.join("model.safetensors")],
                    dtype,
                    &device,
                )
            }
        } else {
            VarBuilder::from_pth(model_path.join("pytorch_model.bin"), dtype, &device)
        }
        .s()?;

        let model: Box<dyn Model + Send> = match device {
            Device::Cpu | Device::Metal(_) => {
                if config.position_embedding_type == PositionEmbeddingType::Alibi {
                    tracing::info!("Starting JinaBert model on {:?}", device);
                    Box::new(JinaBertModel::load(vb, &config, model_type).s()?)
                } else {
                    tracing::info!("Starting Bert model on {:?}", device);
                    Box::new(BertModel::load(vb, &config, model_type).s()?)
                }
            }
            Device::Cuda(_) => {
                #[cfg(not(feature = "cuda"))]
                return Err(BackendError::Start(
                    "`cuda` feature is not enabled".to_string(),
                ));
                #[cfg(feature = "cuda")]
                {
                    if incompatible_compute_cap() {
                        return Err(BackendError::Start(format!("Runtime compute cap {} is not compatible with compile time compute cap {}", get_runtime_compute_cap(), get_compile_compute_cap())));
                    }

                    if cfg!(any(feature = "flash-attn", feature = "flash-attn-v1"))
                        && dtype == DType::F16
                        && config.position_embedding_type == PositionEmbeddingType::Absolute
                        // Allow disabling because of flash attention v1 precision problems
                        // See: https://github.com/huggingface/text-embeddings-inference/issues/37
                        && &std::env::var("USE_FLASH_ATTENTION").unwrap_or("True".to_string()).to_lowercase() == "true"
                    {
                        tracing::info!("Starting FlashBert model on Cuda");
                        Box::new(FlashBertModel::load(vb, &config, model_type).s()?)
                    } else if cfg!(feature = "flash-attn")
                        && dtype == DType::F16
                        && config.position_embedding_type == PositionEmbeddingType::Alibi
                        && &std::env::var("USE_FLASH_ATTENTION")
                            .unwrap_or("True".to_string())
                            .to_lowercase()
                            == "true"
                    {
                        tracing::info!("Starting FlashJinaBertModel model on Cuda");
                        Box::new(FlashJinaBertModel::load(vb, &config, model_type).s()?)
                    } else if config.position_embedding_type == PositionEmbeddingType::Alibi {
                        tracing::info!("Starting JinaBert model on Cuda");
                        Box::new(JinaBertModel::load(vb, &config, model_type).s()?)
                    } else {
                        tracing::info!("Starting Bert model on Cuda");
                        Box::new(BertModel::load(vb, &config, model_type).s()?)
                    }
                }
            }
        };

        Ok(Self { model })
    }
}

impl Backend for CandleBackend {
    fn health(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn is_padded(&self) -> bool {
        self.model.is_padded()
    }

    fn embed(&self, batch: Batch) -> Result<Embeddings, BackendError> {
        let batch_size = batch.len();
        let pooled_indices = batch.pooled_indices.clone();
        let raw_indices = batch.raw_indices.clone();

        // Used for indexing in the raw_embeddings tensor
        let input_lengths: Vec<usize> = (0..batch.len())
            .map(|i| {
                (batch.cumulative_seq_lengths[i + 1] - batch.cumulative_seq_lengths[i]) as usize
            })
            .collect();

        // Run forward
        let (pooled_embeddings, raw_embeddings) = self.model.embed(batch).e()?;

        // Device => Host data transfer
        let pooled_embeddings = match pooled_embeddings {
            None => vec![],
            Some(pooled_embeddings) => pooled_embeddings.to_dtype(DType::F32).e()?.to_vec2().e()?,
        };

        // This transfer is expensive...
        let raw_embeddings = match raw_embeddings {
            None => vec![],
            Some(raw_embeddings) => raw_embeddings.to_dtype(DType::F32).e()?.to_vec2().e()?,
        };

        let mut embeddings =
            HashMap::with_capacity_and_hasher(batch_size, BuildNoHashHasher::default());
        for (i, e) in pooled_indices.into_iter().zip(pooled_embeddings) {
            embeddings.insert(i as usize, Embedding::Pooled(e));
        }

        let mut cumulative_length = 0;
        for i in raw_indices.into_iter() {
            let length = input_lengths[i as usize];
            let e = raw_embeddings[cumulative_length..cumulative_length + length].to_vec();
            embeddings.insert(i as usize, Embedding::All(e));
            cumulative_length += length;
        }

        Ok(embeddings)
    }

    fn predict(&self, batch: Batch) -> Result<Predictions, BackendError> {
        let batch_size = batch.len();

        let results = self.model.predict(batch).e()?;
        let results = results.to_dtype(DType::F32).e()?.to_vec2().e()?;

        let mut predictions =
            HashMap::with_capacity_and_hasher(batch_size, BuildNoHashHasher::default());
        for (i, r) in results.into_iter().enumerate() {
            predictions.insert(i, r);
        }

        Ok(predictions)
    }
}

pub trait WrapErr<O> {
    fn s(self) -> Result<O, BackendError>;
    fn e(self) -> Result<O, BackendError>;
}

impl<O> WrapErr<O> for Result<O, candle::Error> {
    fn s(self) -> Result<O, BackendError> {
        self.map_err(|e| BackendError::Start(e.to_string()))
    }
    fn e(self) -> Result<O, BackendError> {
        self.map_err(|e| BackendError::Inference(e.to_string()))
    }
}
