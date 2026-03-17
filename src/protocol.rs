use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::types::{AskEvent, AskResponse};

pub fn send_wire_message(host: &str, port: u16, req: Value, timeout_s: f64) -> Result<Value> {
    let timeout = Duration::from_secs_f64(timeout_s.max(0.1));
    let mut reader = connect_and_send(host, port, req, timeout_s)?;
    let line = read_line_with_retry(&mut reader, timeout).context("read response failed")?;
    let val: Value = serde_json::from_str(&line).context("invalid daemon response json")?;
    Ok(val)
}

pub fn connect_and_send(
    host: &str,
    port: u16,
    req: Value,
    timeout_s: f64,
) -> Result<BufReader<TcpStream>> {
    let timeout = Duration::from_secs_f64(timeout_s.max(0.1));
    let mut stream = TcpStream::connect((host, port))
        .with_context(|| format!("connect daemon failed: {}:{}", host, port))?;
    stream
        .set_read_timeout(Some(timeout))
        .context("set read timeout failed")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set write timeout failed")?;

    write_json_value_line(&mut stream, &req)?;
    Ok(BufReader::new(stream))
}

fn read_line_with_retry<R: BufRead>(reader: &mut R, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => bail!("daemon returned empty response"),
            Ok(_) => return Ok(line),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(err).context("response wait timeout");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err).context("read line failed"),
        }
    }
}

pub fn write_json_line(stream: &mut TcpStream, resp: &AskResponse) -> Result<()> {
    let data = serde_json::to_vec(resp).context("serialize ask response failed")?;
    stream.write_all(&data).context("write response failed")?;
    stream.write_all(b"\n").context("write newline failed")?;
    stream.flush().context("flush response failed")?;
    Ok(())
}

pub fn write_json_event_line(stream: &mut TcpStream, evt: &AskEvent) -> Result<()> {
    let data = serde_json::to_vec(evt).context("serialize ask event failed")?;
    stream.write_all(&data).context("write event failed")?;
    stream.write_all(b"\n").context("write newline failed")?;
    stream.flush().context("flush event failed")?;
    Ok(())
}

pub fn write_json_value_line(stream: &mut TcpStream, val: &Value) -> Result<()> {
    let data = serde_json::to_vec(val).context("serialize json value failed")?;
    stream.write_all(&data).context("write value failed")?;
    stream.write_all(b"\n").context("write newline failed")?;
    stream.flush().context("flush value failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct FlakyReader {
        steps: Vec<io::Result<Vec<u8>>>,
        offset: usize,
    }

    impl Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.offset >= self.steps.len() {
                return Ok(0);
            }
            match &self.steps[self.offset] {
                Ok(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    self.offset += 1;
                    Ok(n)
                }
                Err(err) => {
                    self.offset += 1;
                    Err(io::Error::new(err.kind(), err.to_string()))
                }
            }
        }
    }

    #[test]
    fn read_line_with_retry_handles_would_block_then_data() {
        let reader = FlakyReader {
            steps: vec![
                Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Ok(b"{\"type\":\"ask.response\",\"reply\":\"OK\"}\n".to_vec()),
            ],
            offset: 0,
        };
        let mut reader = BufReader::new(reader);
        let line = read_line_with_retry(&mut reader, Duration::from_millis(200)).expect("line");
        assert!(line.contains("\"ask.response\""));
    }
}
