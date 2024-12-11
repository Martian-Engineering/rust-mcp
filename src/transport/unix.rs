use std::path::PathBuf;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::{timeout, Duration},
};
use async_trait::async_trait;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use crate::error::McpError;
use super::{
    Transport,
    TransportChannels,
    TransportCommand,
    TransportEvent,
    JsonRpcMessage,
};

pub struct UnixTransport {
    path: PathBuf,
    server_mode: bool,
    buffer_size: usize,
}

impl UnixTransport {
    pub fn new_server(path: PathBuf, buffer_size: Option<usize>) -> Self {
        Self {
            path,
            server_mode: true,
            buffer_size: buffer_size.unwrap_or(4092),
        }
    }

    pub fn new_client(path: PathBuf, buffer_size: Option<usize>) -> Self {
        Self {
            path,
            server_mode: false,
            buffer_size: buffer_size.unwrap_or(4092),
        }
    }

    async fn handle_connection(
        stream: UnixStream,
        cmd_rx: mpsc::Receiver<TransportCommand>,
        event_tx: mpsc::Sender<TransportEvent>,
    ) {
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = writer;
        let (write_tx, mut write_rx) = mpsc::channel::<String>(32);

        // Writer task
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if !msg.contains("notifications/message") && !msg.contains("list_changed") {
                    tracing::debug!("-> {}", msg);
                }

                if let Err(e) = async {
                    writer.write_all(msg.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                    Ok::<_, std::io::Error>(())
                }.await {
                    tracing::error!("Write error: {:?}", e);
                    break;
                }
            }
        });

        // Reader task
        let reader_handle = tokio::spawn({
            let event_tx = event_tx.clone();
            async move {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.contains("notifications/message") && !trimmed.contains("list_changed") {
                                tracing::debug!("<- {}", trimmed);
                            }

                            if !trimmed.is_empty() {
                                match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                                    Ok(msg) => {
                                        if event_tx.send(TransportEvent::Message(msg)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Parse error: {}, input: {}", e, trimmed);
                                        if event_tx.send(TransportEvent::Error(McpError::ParseError)).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Read error: {:?}", e);
                            let _ = event_tx.send(TransportEvent::Error(McpError::IoError)).await;
                            break;
                        }
                    }
                }
            }
        });

        // Main message loop
        let mut cmd_rx = cmd_rx;
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TransportCommand::SendMessage(msg) => {
                    match serde_json::to_string(&msg) {
                        Ok(s) => {
                            if write_tx.send(s).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::error!("Failed to serialize message: {:?}", e),
                    }
                }
                TransportCommand::Close => {
                    // Just break the loop - write_tx will be dropped after the loop
                    break;
                }
            }
        }

        // Cleanup
        drop(write_tx);  // This ensures pending messages are sent before closing
        let _ = reader_handle.await;
        let _ = writer_handle.await;
        let _ = event_tx.send(TransportEvent::Closed).await;
    }

    async fn run_server(
        path: PathBuf,
        cmd_rx: mpsc::Receiver<TransportCommand>,
        event_tx: mpsc::Sender<TransportEvent>,
    ) {
        tracing::debug!("Server task started");
        
        // Create and bind to the Unix socket
        tracing::debug!("Creating Unix socket");
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("Failed to bind Unix socket: {}", e);
                let _ = event_tx.send(TransportEvent::Error(McpError::IoError)).await;
                return;
            }
        };

        // Set socket file permissions to rw-rw----
        tracing::debug!("Setting socket permissions to 0o660");
        if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o660)) {
            tracing::error!("Failed to set socket permissions: {}", e);
            let _ = event_tx.send(TransportEvent::Error(McpError::IoError)).await;
            return;
        }

        tracing::debug!("Waiting for connection");
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tracing::debug!("Connection accepted");
                Self::handle_connection(stream, cmd_rx, event_tx).await;
            }
            Err(e) => {
                tracing::error!("Failed to accept connection: {:?}", e);
                let _ = event_tx.send(TransportEvent::Error(McpError::IoError)).await;
            }
        }

        tracing::debug!("Cleaning up socket file");
        let _ = std::fs::remove_file(path);
    }

    async fn run_client(
        path: PathBuf,
        cmd_rx: mpsc::Receiver<TransportCommand>,
        event_tx: mpsc::Sender<TransportEvent>,
    ) {
        // Connect to the Unix socket
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                Self::handle_connection(stream, cmd_rx, event_tx).await;
            }
            Err(e) => {
                tracing::error!("Failed to connect to Unix socket: {:?}", e);
                let _ = event_tx.send(TransportEvent::Error(McpError::IoError)).await;
            }
        }
    }
}

#[async_trait]
impl Transport for UnixTransport {
    async fn start(&mut self) -> Result<TransportChannels, McpError> {
        tracing::debug!("Transport start called, server_mode: {}", self.server_mode);
        let (cmd_tx, cmd_rx) = mpsc::channel(self.buffer_size);
        let (event_tx, event_rx) = mpsc::channel(self.buffer_size);

        if self.server_mode {
            if let Some(parent) = self.path.parent() {
                tracing::debug!("Creating directory: {:?}", parent);
                fs::create_dir_all(parent).map_err(|e| {
                    tracing::error!("Failed to create directory {:?}: {}", parent, e);
                    McpError::IoError
                })?;
                
                // Only set permissions if we created the directory
                // Skip permission setting for system directories like /tmp
                if parent.starts_with("/tmp/mcp") {
                    tracing::debug!("Setting directory permissions to 0o755");
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).map_err(|e| {
                        tracing::error!("Failed to set directory permissions: {}", e);
                        McpError::IoError
                    })?;
                }
            }
            
            if self.path.exists() {
                tracing::debug!("Removing existing socket file");
                fs::remove_file(&self.path).map_err(|e| {
                    tracing::error!("Failed to remove existing socket: {}", e);
                    McpError::IoError
                })?;
            }
        }

        if self.server_mode {
            tokio::spawn(Self::run_server(
                self.path.clone(),
                cmd_rx,
                event_tx,
            ));
        } else {
            tokio::spawn(Self::run_client(
                self.path.clone(),
                cmd_rx,
                event_tx,
            ));
        }

        let event_rx = Arc::new(tokio::sync::Mutex::new(event_rx));
        Ok(TransportChannels { cmd_tx, event_rx })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcNotification;
    use std::time::Duration;
    use tokio::time::sleep;
    use tracing_subscriber::fmt::format::FmtSpan;

    #[tokio::test]
    async fn test_unix_transport() -> Result<(), McpError> {
        // Initialize logging for tests
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_span_events(FmtSpan::FULL)
            .try_init();

        tokio::time::timeout(Duration::from_secs(5), async {
            tracing::info!("Starting test");
            // Use a subdirectory in /tmp
            let socket_path = PathBuf::from("/tmp/mcp-test/test_socket");
            
            tracing::info!("Creating transports");
            let mut server = UnixTransport::new_server(socket_path.clone(), Some(4092));
            let mut client = UnixTransport::new_client(socket_path.clone(), Some(4092));

            tracing::info!("Starting server");
            let server_channels = server.start().await?;
            
            tracing::info!("Waiting before starting client");
            sleep(Duration::from_millis(100)).await;
            
            tracing::info!("Starting client");
            let client_channels = client.start().await?;

            // Send test messages
            let test_msg = JsonRpcMessage::Notification(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "test".to_string(),
                params: None,
            });

            client_channels.cmd_tx.send(TransportCommand::SendMessage(test_msg.clone())).await.unwrap();
            server_channels.cmd_tx.send(TransportCommand::SendMessage(test_msg)).await.unwrap();

            // Wait for messages to be processed
            sleep(Duration::from_millis(100)).await;

            // Send close commands
            client_channels.cmd_tx.send(TransportCommand::Close).await.unwrap();
            server_channels.cmd_tx.send(TransportCommand::Close).await.unwrap();

            // Wait for cleanup
            sleep(Duration::from_millis(100)).await;

            // Verify socket file is cleaned up
            assert!(!socket_path.exists());

            Ok(())
        })
        .await
        .map_err(|_| McpError::ShutdownTimeout)?
    }
} 