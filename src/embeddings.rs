use crate::{doc_loader::Document, error::ServerError};
use async_openai::error::ApiError as OpenAIAPIErr;
use ndarray::{Array1, ArrayView1};
use std::sync::OnceLock;
use std::sync::Arc;
use tiktoken_rs::cl100k_base;
use futures::stream::{self, StreamExt};

// Static OnceLock for the OpenAI client
pub static OPENAI_CLIENT: OnceLock<async_openai::Client<async_openai::config::OpenAIConfig>> = OnceLock::new();
pub static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
pub static EMBEDDING_API_BASE: OnceLock<String> = OnceLock::new();


use bincode::{Encode, Decode};
use serde::{Serialize, Deserialize};

// Define a struct containing path, content, and embedding for caching
#[derive(Serialize, Deserialize, Debug, Encode, Decode)]
pub struct CachedDocumentEmbedding {
    pub path: String,
    pub content: String, // Add the extracted document content
    pub vector: Vec<f32>,
}


/// Calculates the cosine similarity between two vectors.
pub fn cosine_similarity(v1: ArrayView1<f32>, v2: ArrayView1<f32>) -> f32 {
    let dot_product = v1.dot(&v2);
    let norm_v1 = v1.dot(&v1).sqrt();
    let norm_v2 = v2.dot(&v2).sqrt();

    if norm_v1 == 0.0 || norm_v2 == 0.0 {
        0.0
    } else {
        dot_product / (norm_v1 * norm_v2)
    }
}

/// Generates embeddings for a list of documents using the OpenAI API.
pub async fn generate_embeddings(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    documents: &[Document],
    model: &str,
) -> Result<(Vec<(String, Array1<f32>)>, usize), ServerError> {
    let bpe = Arc::new(cl100k_base().map_err(|e| ServerError::Tiktoken(e.to_string()))?);

    const CONCURRENCY_LIMIT: usize = 8;
    const TOKEN_LIMIT: usize = 450; // nomic-embed-text batch limit is 512

    let url = format!("{}/embeddings", api_base);

    let results = stream::iter(documents.iter().enumerate())
        .map(|(index, doc)| {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.to_string();
            let model = model.to_string();
            let doc = doc.clone();
            let bpe = Arc::clone(&bpe);

            async move {
                let token_count = bpe.encode_with_special_tokens(&doc.content).len();
                if token_count > TOKEN_LIMIT {
                    return Ok::<Option<(String, Array1<f32>, usize)>, ServerError>(None);
                }

                let body = serde_json::json!({
                    "model": model,
                    "input": [doc.content]
                });

                let resp = client.post(&url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ServerError::OpenAI(async_openai::error::OpenAIError::Reqwest(e)))?;

                let status = resp.status();
                let bytes = resp.bytes().await
                    .map_err(|e| ServerError::OpenAI(async_openai::error::OpenAIError::Reqwest(e)))?;

                if !status.is_success() {
                    let msg = String::from_utf8_lossy(&bytes).into_owned();
                    // Skip documents that exceed the batch size limit
                    if msg.contains("too large to process") || msg.contains("batch size") {
                        return Ok(None);
                    }
                    return Err(ServerError::OpenAI(async_openai::error::OpenAIError::ApiError(
                        async_openai::error::ApiError {
                            message: msg, r#type: None, param: None, code: None,
                        }
                    )));
                }

                let response: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ServerError::OpenAI(async_openai::error::OpenAIError::JSONDeserialize(e)))?;

                let data = response["data"].as_array()
                    .ok_or_else(|| ServerError::OpenAI(async_openai::error::OpenAIError::ApiError(
                        async_openai::error::ApiError {
                            message: "missing data array".into(), r#type: None, param: None, code: None,
                        }
                    )))?;

                if data.len() != 1 {
                    return Err(ServerError::OpenAI(async_openai::error::OpenAIError::ApiError(
                        async_openai::error::ApiError {
                            message: format!("expected 1 embedding, got {}", data.len()),
                            r#type: None, param: None, code: None,
                        }
                    )));
                }

                let embedding: Vec<f32> = serde_json::from_value(data[0]["embedding"].clone())
                    .map_err(|e| ServerError::OpenAI(async_openai::error::OpenAIError::JSONDeserialize(e)))?;

                let embedding_array = Array1::from(embedding);
                Ok(Some((doc.path.clone(), embedding_array, token_count)))
            }
        })
        .buffer_unordered(CONCURRENCY_LIMIT)
        .collect::<Vec<Result<Option<(String, Array1<f32>, usize)>, ServerError>>>()
        .await;

    // Process collected results, filtering out errors and skipped documents, summing tokens
    let mut embeddings_vec = Vec::new();
    let mut total_processed_tokens: usize = 0;
    for result in results {
        match result {
            Ok(Some((path, embedding, tokens))) => {
                embeddings_vec.push((path, embedding)); // Keep successful embeddings
                total_processed_tokens += tokens; // Add tokens for successful ones
            }
            Ok(None) => {} // Ignore skipped documents
            Err(e) => {
                // Log error but potentially continue? Or return the first error?
                // For now, let's return the first error encountered.
                eprintln!("Error during concurrent embedding generation: {}", e);
                return Err(e);
            }
        }
    }

    eprintln!(
        "Finished generating embeddings. Successfully processed {} documents ({} tokens).",
        embeddings_vec.len(), total_processed_tokens
    );
    Ok((embeddings_vec, total_processed_tokens)) // Return tuple
}