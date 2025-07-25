use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use log::{info, warn};
use opendal::{
    layers::LoggingLayer,
    services::{Fs, S3},
    BufferStream, Operator,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::Mutex;

use crate::{common::extract_timestamp_from_filename, storage::Entry};

use super::io::{StorageReader, StorageWriter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageCredentials {
    None,
    Basic {
        username: String,
        password: String,
    },
    AccessKey {
        access_key: String,
        secret_key: String,
    },
    PrivateKey {
        username: String,
        key_path: String,
        passphrase: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageType {
    FileSystem,
    S3,
    // WebDAV,
    // SFTP,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalStorageConfig {
    pub id: String,
    pub name: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3StorageConfig {
    pub id: String,
    pub name: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageConfig {
    Local(LocalStorageConfig),
    S3(S3StorageConfig),
}

#[derive(Clone)]
pub struct StorageProvider {
    pub config: StorageConfig,
    pub operator: Operator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOptions {
    pub latest_only: Option<bool>,
    pub limit: Option<usize>,
}

impl StorageProvider {
    pub fn new(config: StorageConfig) -> anyhow::Result<Self> {
        let operator = match &config {
            StorageConfig::Local(config) => {
                let builder = Fs::default().root(&config.location);
                Operator::new(builder)?
                    .layer(LoggingLayer::default())
                    .finish()
            }
            StorageConfig::S3(config) => {
                let mut builder = S3::default()
                    .root(&config.location)
                    .bucket(&config.bucket)
                    .region(&config.region)
                    .access_key_id(&config.access_key)
                    .secret_access_key(&config.secret_key);

                builder = match &config.endpoint {
                    Some(endpoint) => builder.endpoint(endpoint),
                    None => builder,
                };

                Operator::new(builder)?
                    .layer(LoggingLayer::default())
                    .finish()
            }
        };

        Ok(StorageProvider { config, operator })
    }

    pub async fn test(&self) -> Result<bool> {
        self.operator
            .list_with("/")
            .recursive(true)
            .limit(1)
            .await?;

        Ok(true)
    }

    pub async fn list(&self) -> Result<Vec<Entry>> {
        self.list_with_options(ListOptions {
            latest_only: None,
            limit: None,
        })
        .await
    }

    pub async fn list_with_options(&self, options: ListOptions) -> Result<Vec<Entry>> {
        let limit = options.limit.unwrap_or(1000);
        let latest_only = options.latest_only.unwrap_or(false);

        let result = self
            .operator
            .list_with("")
            .recursive(true)
            .limit(limit)
            .await
            .context(format!("Failed to list backups"))?;

        let mut filtered_results: Vec<Entry> = result
            .into_iter()
            .map(|opendal_entry| {
                let mut entry = Entry::from(&opendal_entry);
                entry.metadata.content_length = self.get_content_length(&entry);
                entry
            })
            .filter(|entry| entry.metadata.is_file)
            .collect();

        filtered_results.sort_by(|a, b| {
            let a_timestamp =
                extract_timestamp_from_filename(&a.metadata.name).unwrap_or(DateTime::default());

            let b_timestamp =
                extract_timestamp_from_filename(&b.metadata.name).unwrap_or(DateTime::default());

            b_timestamp.cmp(&a_timestamp)
        });

        if latest_only {
            match filtered_results.first() {
                Some(entry) => return Ok(vec![entry.clone()]),
                None => return Err(anyhow!("No entry found")),
            }
        }

        Ok(filtered_results)
    }

    pub async fn create_writer(&self, filename: &str) -> Result<Box<dyn Write + Send + Unpin>> {
        let op_writer = self.operator.writer(filename).await?;
        Ok(Box::new(StorageWriter::new(op_writer)))
    }

    pub async fn create_stream(&self, filename: &str) -> Result<Arc<Mutex<BufferStream>>> {
        let metadata = self.operator.stat(filename).await?;
        let file_size = metadata.content_length() as usize;
        let chunk_size = if file_size > 512 { 512 } else { file_size };

        let stream = self
            .operator
            .reader_with(filename)
            .chunk(chunk_size as usize)
            .await?
            .into_stream(0u64..(file_size as u64))
            .await?;

        Ok(Arc::new(Mutex::new(stream)))
    }

    pub async fn create_reader(&self, filename: &str) -> Result<Box<dyn Read + Send + Unpin>> {
        Ok(Box::new(StorageReader::new(
            self.operator.clone(),
            filename.to_string(),
        )))
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.operator
            .delete(&path)
            .await
            .context(format!("Failed to delete backup {}", path))?;

        Ok(())
    }

    pub async fn cleanup(&self, retention_days: u64, dry_run: bool) -> Result<(usize, u64)> {
        let backups = self.list().await?;

        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(retention_days * 86400))
            .ok_or_else(|| anyhow!("Failed to calculate cutoff date"))?;

        let cutoff_datetime: DateTime<Utc> = cutoff.into();

        let mut deleted_count = 0;
        let mut deleted_size = 0;

        for backup in backups {
            match extract_timestamp_from_filename(&backup.metadata.name) {
                Ok(timestamp) => {
                    if timestamp < cutoff_datetime {
                        let size = backup.metadata.content_length;
                        deleted_size += size;
                        deleted_count += 1;

                        if !dry_run {
                            self.delete(&backup.path).await?;
                            info!("Successfully deleted {}", backup.path);
                        }
                    }
                }
                Err(_) => {
                    warn!("Failed to extract timestamp from {}", backup.metadata.name);
                }
            };
        }

        Ok((deleted_count, deleted_size))
    }

    fn get_content_length(&self, entry: &Entry) -> u64 {
        match &self.config {
            StorageConfig::Local(local_config) => {
                let full_path = Path::new(&local_config.location).join(&entry.path);
                let content_length = match fs::metadata(&full_path).context(format!(
                    "Failed to get metadata for {}",
                    full_path.display()
                )) {
                    Ok(metadata) => metadata.len(),
                    Err(_) => 0,
                };

                content_length
            }
            _ => entry.metadata.content_length,
        }
    }
}
