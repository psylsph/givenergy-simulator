//! Modbus TCP emulation: TCP listener, session manager, register store bridge.
//!
//! Phase 1 MVP: basic TCP skeleton that serves register values.
//! Phase 3 will match real GivEnergy timing and register scaling.

use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// MBAP header size for Modbus TCP.
const MBAP_HEADER_SIZE: usize = 7;

/// Run a basic Modbus TCP server that serves register values.
///
/// For MVP this handles Read Holding Registers (function code 0x03) only.
pub async fn run_modbus_server(
    addr: SocketAddr,
    register_store: std::sync::Arc<tokio::sync::Mutex<sim_registers::RegisterStore>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Modbus TCP server listening on {addr}");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        tracing::debug!("Modbus connection from {peer}");
        let store = register_store.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 260];
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("Modbus read error from {peer}: {e}");
                        break;
                    }
                };

                if n < MBAP_HEADER_SIZE {
                    continue;
                }

                // Parse function code
                let func = buf[7];
                if func == 0x03 {
                    // Read Holding Registers
                    let start_addr = u16::from_be_bytes([buf[8], buf[9]]);
                    let count = u16::from_be_bytes([buf[10], buf[11]]);

                    let store = store.lock().await;
                    let mut response_bytes = Vec::with_capacity(count as usize * 2);
                    for i in 0..count {
                        let val = store.read(start_addr + i).unwrap_or(0);
                        response_bytes.extend_from_slice(&val.to_be_bytes());
                    }

                    // Build response: reuse transaction ID from request
                    let byte_count = response_bytes.len() as u8;
                    let mut resp = Vec::with_capacity(MBAP_HEADER_SIZE + 2 + response_bytes.len());
                    // MBAP header (7 bytes)
                    resp.extend_from_slice(&buf[0..4]); // transaction ID + protocol ID
                    let length = (2 + 1 + 1 + response_bytes.len()) as u16;
                    resp.extend_from_slice(&length.to_be_bytes()); // length
                    resp.push(buf[6]); // unit ID
                    resp.push(func);   // function code
                    resp.push(byte_count);
                    resp.extend_from_slice(&response_bytes);

                    let _ = stream.write_all(&resp).await;
                }
                // Other function codes: silently ignored in MVP
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Integration tests for Modbus will use real TCP connections
        // in the test matrix (see docs/14-testing-matrix.md).
    }
}
