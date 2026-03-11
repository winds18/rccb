use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::types::{AskEvent, AskResponse};

pub fn send_wire_message(host: &str, port: u16, req: Value, timeout_s: f64) -> Result<Value> {
    let mut reader = connect_and_send(host, port, req, timeout_s)?;
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("read response failed")?;
    if n == 0 {
        bail!("daemon returned empty response");
    }

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
