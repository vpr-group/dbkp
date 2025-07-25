use std::{
    io::{Read, Write},
    process::Stdio,
    time::Duration,
};

use crate::databases::{
    ssh_tunnel::{SshRemoteConfig, SshTunnel},
    version::{Version, VersionTrait},
    DatabaseConfig, DatabaseConnectionTrait, DatabaseMetadata, RestoreOptions, UtilitiesTrait,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    Pool, Postgres,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use super::{utilities::PostgreSqlUtilities, version::PostgreSQLVersion};

pub struct PostgreSqlConnection {
    pub config: DatabaseConfig,
    pub pool: Pool<Postgres>,
    _ssh_tunnel: Option<SshTunnel>,
}

impl PostgreSqlConnection {
    pub async fn new(config: DatabaseConfig) -> Result<Self> {
        let mut config = config.clone();
        let ssh_tunnel = match &config.ssh_tunnel {
            Some(ssh_config) => {
                let tunnel = SshTunnel::new(
                    ssh_config.clone(),
                    SshRemoteConfig {
                        host: config.host.clone(),
                        port: config.port,
                    },
                )?;

                Some(tunnel)
            }
            None => None,
        };

        if let Some(ssh_tunnel) = &ssh_tunnel {
            config.host = "localhost".into();
            config.port = ssh_tunnel.local_port;
        }

        let mut connect_options = PgConnectOptions::new()
            .host(&config.host)
            .username(&config.username)
            .database("postgres")
            .port(config.port);

        connect_options = match &config.password {
            Some(password) => connect_options.password(&password),
            None => connect_options,
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(connect_options)
            .await?;

        Ok(Self {
            config,
            pool,
            _ssh_tunnel: ssh_tunnel,
        })
    }

    async fn get_base_command(&self, bin_name: &str) -> Result<Command> {
        let metadata = self.get_metadata().await?;
        let version = match metadata.version {
            Version::PostgreSQL(version) => version,
            _ => return Err(anyhow!("Wrong version type")),
        };

        let utilities = PostgreSqlUtilities::new(version);
        let mut cmd = utilities.get_command(bin_name).await?;

        if let Some(pass) = &self.config.password {
            cmd.env("PGPASSWORD", pass);
        }

        Ok(cmd)
    }

    async fn get_command(&self, bin_name: &str) -> Result<Command> {
        let mut cmd = self.get_base_command(bin_name).await?;

        cmd.arg("-h")
            .arg(&self.config.host)
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg("-U")
            .arg(&self.config.username)
            .arg("-d")
            .arg(&self.config.database);

        Ok(cmd)
    }
}

#[async_trait]
impl DatabaseConnectionTrait for PostgreSqlConnection {
    async fn get_metadata(&self) -> Result<DatabaseMetadata> {
        let version_string: (String,) = sqlx::query_as("SELECT version()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| anyhow!("Failed to get database version: {}", e))?;

        let version = match PostgreSQLVersion::parse_string_version(version_string.0.as_str()) {
            Some(version) => version,
            None => return Err(anyhow!("Fauiled to parse PostgreSQL version string")),
        };

        Ok(DatabaseMetadata {
            version: Version::PostgreSQL(version),
        })
    }

    async fn test(&self) -> Result<bool> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| true)
            .map_err(|e| anyhow!("Connection test failed: {}", e))
    }

    async fn backup(&self, writer: &mut (dyn Write + Send + Unpin)) -> Result<()> {
        let mut cmd = self.get_command("pg_dump").await?;

        cmd.arg("--format=plain")
            .arg("--encoding=UTF8")
            .arg("--schema=*")
            .arg("--clean")
            .arg("--if-exists")
            .arg("--no-owner")
            .arg("--blobs")
            .arg("--exclude-schema=information_schema")
            .arg("--exclude-schema=pg_catalog")
            .arg("--exclude-schema=pg_toast")
            .arg("--exclude-schema=pg_temp*")
            .arg("--exclude-schema=pg_toast_temp*");

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to start pg_dump: {}", e))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture pg_dump stdout".to_string()))?;

        let mut buffer = [0u8; 16384];

        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    writer
                        .write_all(&buffer[..n])
                        .map_err(|e| anyhow!("Failed to write backup data: {}", e))?;
                }
                Err(e) => {
                    return Err(anyhow!("Failed to read from pg_dump: {}", e));
                }
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| anyhow!("pg_dump process failed: {}", e))?;

        if !status.success() {
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow!("Failed to capture pg_dump stderr".to_string()))?;

            let mut error_message = String::new();
            stderr
                .read_to_string(&mut error_message)
                .await
                .map_err(|e| anyhow!("Failed to read pg_dump stderr: {}", e))?;

            return Err(anyhow!("pg_dump failed: {}", error_message));
        }

        Ok(())
    }

    async fn restore(&self, reader: &mut (dyn Read + Send + Unpin)) -> Result<()> {
        self.restore_with_options(
            reader,
            RestoreOptions {
                drop_database_first: true,
            },
        )
        .await
    }

    async fn restore_with_options(
        &self,
        reader: &mut (dyn Read + Send + Unpin),
        options: RestoreOptions,
    ) -> Result<()> {
        let mut cmd = self.get_base_command("psql").await?;

        cmd.arg("-h")
            .arg(&self.config.host)
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg("-U")
            .arg(&self.config.username)
            .arg("-d")
            .arg("postgres")
            .arg("-c")
            .arg(format!(
                "SELECT pg_terminate_backend(pg_stat_activity.pid) 
                FROM pg_stat_activity 
                WHERE pg_stat_activity.datname = '{}' 
                AND pid <> pg_backend_pid();",
                self.config.database
            ));

        let drop_connections_output = cmd
            .output()
            .await
            .context("Failed to execute connection termination command")?;

        if !drop_connections_output.status.success() {
            let stderr = String::from_utf8_lossy(&drop_connections_output.stderr);
            let exit_code = drop_connections_output.status.code().unwrap_or(-1);

            return Err(anyhow!(
                "Failed to terminate database connections with exit code {}.\nError details: {}",
                exit_code,
                stderr.trim()
            ));
        }

        if options.drop_database_first {
            let mut cmd = self.get_base_command("psql").await?;

            cmd.arg("-h")
                .arg(&self.config.host)
                .arg("-p")
                .arg(self.config.port.to_string())
                .arg("-U")
                .arg(&self.config.username)
                .arg("-d")
                .arg("postgres")
                .arg("-c")
                .arg(format!(
                    "DROP DATABASE IF EXISTS \"{}\";",
                    self.config.database
                ));

            let output = cmd
                .output()
                .await
                .context("Failed to execute drop database command")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                return Err(anyhow!(
                    "Failed to drop database with exit code {}.\nError: {}",
                    exit_code,
                    stderr.trim()
                ));
            }

            let mut create_cmd = self.get_base_command("psql").await?;

            create_cmd
                .arg("-h")
                .arg(&self.config.host)
                .arg("-p")
                .arg(self.config.port.to_string())
                .arg("-U")
                .arg(&self.config.username)
                .arg("-d")
                .arg("postgres")
                .arg("-c")
                .arg(format!("CREATE DATABASE \"{}\";", self.config.database));

            let create_output = create_cmd
                .output()
                .await
                .context("Failed to create database")?;

            if !create_output.status.success() {
                let stderr = String::from_utf8_lossy(&create_output.stderr);
                let exit_code = create_output.status.code().unwrap_or(-1);

                return Err(anyhow!(
                    "Failed to create database with exit code {}.\nError: {}",
                    exit_code,
                    stderr.trim()
                ));
            }
        }

        let mut cmd = self.get_base_command("psql").await?;

        cmd.arg("-h")
            .arg(&self.config.host)
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg("-U")
            .arg(&self.config.username)
            .arg("-d")
            .arg(&self.config.database);

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to capture psql stdin"))?;

        let mut buffer = [0u8; 16384];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    stdin.write_all(&buffer[..n]).await?;
                }
                Err(e) => {
                    return Err(anyhow!("Failed to read backup data: {}", e));
                }
            }
        }

        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| anyhow!("psql process failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let exit_code = output.status.code().unwrap_or(-1);

            return Err(anyhow!(
                "psql restore failed with exit code {}.\nStderr: {}\nStdout: {}",
                exit_code,
                stderr.trim(),
                stdout.trim()
            ));
        }

        Ok(())
    }
}
