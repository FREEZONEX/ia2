//! WebSocket bridge to a per-connection ironplc LSP child process.

use std::path::PathBuf;
use std::process::Stdio;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt as FuturesStreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Ceiling on one LSP message body from the child. Semantic tokens for a
/// large POU are measured in kilobytes; 16 MiB is far above anything real and
/// far below "allocate whatever the stream claims".
const MAX_LSP_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on one header line. `read_line` grows its buffer until a newline
/// arrives, so a child emitting none would grow it without limit.
const MAX_LSP_HEADER_BYTES: usize = 8 * 1024;

/// Upgrade to WebSocket and bridge to a freshly-spawned ironplc LSP
/// process. WS frames are LSP JSON-RPC bodies (no Content-Length header);
/// the proxy adds/strips headers when talking to stdio.
pub async fn lsp(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_lsp_ws)
}

fn lsp_launcher_path() -> PathBuf {
    if let Ok(p) = std::env::var("LSP_LAUNCHER") {
        return PathBuf::from(p);
    }
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.push("lsp-launcher");
    path
}

async fn handle_lsp_ws(mut socket: WebSocket) {
    let cmd = lsp_launcher_path();
    let mut child = match Command::new(&cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %cmd.display(), %e, "failed to spawn lsp-launcher");
            // Close with a reason instead of dropping the socket cold:
            // the browser's devtools then show WHY the LSP died (a
            // missing sidecar binary — i.e. an incomplete install)
            // rather than a bare disconnect. The editor keeps working
            // either way; diagnostics come from /api/check.
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011, // internal error
                    // Close reasons allow only 123 UTF-8 bytes; the full path
                    // is already in the server log and may be arbitrarily long.
                    reason: "lsp-launcher not available — reinstall (the binary ships alongside ia2-server)"
                        .into(),
                })))
                .await;
            return;
        }
    };
    tracing::info!(path = %cmd.display(), "lsp-launcher spawned for ws client");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (mut ws_tx, mut ws_rx) = FuturesStreamExt::split(socket);

    // WS → child stdin: add LSP Content-Length framing.
    let to_child = async move {
        while let Some(msg) = FuturesStreamExt::next(&mut ws_rx).await {
            let Ok(msg) = msg else { break };
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let body = text.as_bytes();
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            if stdin.write_all(header.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(body).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    };

    // child stdout → WS: strip LSP Content-Length framing.
    let from_child = async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_lsp_frame(&mut reader).await {
                Ok(Some(text)) => {
                    if ws_tx.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(why) => {
                    tracing::error!(%why, "lsp stdout stopped being LSP framing; closing the bridge");
                    return;
                }
            }
        }
    };

    tokio::select! {
        _ = to_child => {}
        _ = from_child => {}
    }
    let _ = child.kill().await;
}

/// Read one LSP frame from a child's stdout: headers, then exactly
/// `Content-Length` bytes of body.
///
/// `Ok(None)` is a clean EOF. `Err` means the stream stopped being LSP
/// framing and re-syncing it would be guesswork — the caller closes the
/// bridge rather than guess.
///
/// Both reads are bounded. The child is our own lsp-launcher, so this is not
/// a security boundary — it is blast-radius control. A desynced or buggy
/// child announcing a wild `Content-Length` would make `vec![0u8; len]`
/// allocate it verbatim, and THIS process is also the one holding a running
/// PLC program: an OOM abort here stops the plant, not just the editor's
/// autocomplete.
///
/// A header block with no `Content-Length`, and a body that is not UTF-8,
/// are both skipped rather than fatal — that is the pre-existing behaviour
/// and it costs one message, not the session.
async fn read_lsp_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<String>, String> {
    loop {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = Vec::new();
            let n = match reader
                .take(MAX_LSP_HEADER_BYTES as u64)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) => return Ok(None),
                Ok(n) => n,
                Err(e) => return Err(format!("read: {e}")),
            };
            if n == MAX_LSP_HEADER_BYTES && !line.ends_with(b"\n") {
                return Err(format!("header line exceeded {MAX_LSP_HEADER_BYTES} bytes"));
            }
            let Ok(line) = String::from_utf8(line) else {
                return Err("header line was not UTF-8".into());
            };
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok();
            }
        }
        let Some(len) = content_length else {
            continue;
        };
        if len > MAX_LSP_BODY_BYTES {
            return Err(format!(
                "Content-Length {len} exceeds the {MAX_LSP_BODY_BYTES}-byte ceiling"
            ));
        }
        let mut body = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut body).await {
            return Err(format!("body read: {e}"));
        }
        match String::from_utf8(body) {
            Ok(text) => return Ok(Some(text)),
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod lsp_frame_tests {
    use super::{read_lsp_frame, MAX_LSP_BODY_BYTES, MAX_LSP_HEADER_BYTES};
    use tokio::io::BufReader;

    async fn frames(input: &[u8]) -> Result<Vec<String>, String> {
        let mut r = BufReader::new(std::io::Cursor::new(input.to_vec()));
        let mut out = Vec::new();
        loop {
            match read_lsp_frame(&mut r).await? {
                Some(t) => out.push(t),
                None => return Ok(out),
            }
        }
    }

    #[tokio::test]
    async fn back_to_back_frames_are_split_on_content_length() {
        let got = frames(b"Content-Length: 2\r\n\r\n{}Content-Length: 5\r\n\r\n[1,2]")
            .await
            .expect("well-framed");
        assert_eq!(got, vec!["{}".to_string(), "[1,2]".to_string()]);
    }

    #[tokio::test]
    async fn a_clean_eof_ends_the_stream() {
        assert_eq!(frames(b"").await.unwrap(), Vec::<String>::new());
    }

    /// The lever this guard exists for: `vec![0u8; len]` would have
    /// allocated whatever the stream claimed, in the process that also holds
    /// the running PLC program.
    #[tokio::test]
    async fn an_implausible_content_length_is_refused_not_allocated() {
        let input = format!("Content-Length: {}\r\n\r\n", u32::MAX);
        let err = frames(input.as_bytes()).await.expect_err("must refuse");
        assert!(err.contains("exceeds the"), "{err}");
        assert!(err.contains(&MAX_LSP_BODY_BYTES.to_string()), "{err}");
    }

    /// A child that emits no newline would grow `read_line`'s buffer without
    /// limit — same allocation lever, different door.
    #[tokio::test]
    async fn a_header_line_without_a_newline_is_bounded() {
        let input = vec![b'x'; MAX_LSP_HEADER_BYTES + 10];
        let err = frames(&input).await.expect_err("must refuse");
        assert!(err.contains("header line exceeded"), "{err}");
    }

    /// Pre-existing tolerances kept: a header block with no Content-Length
    /// costs one message, not the session.
    #[tokio::test]
    async fn a_header_block_without_content_length_is_skipped() {
        let got = frames(b"X-Note: hi\r\n\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .expect("skips, then reads");
        assert_eq!(got, vec!["{}".to_string()]);
    }

    #[tokio::test]
    async fn a_body_that_is_not_utf8_costs_one_message_not_the_session() {
        let mut input = b"Content-Length: 2\r\n\r\n".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        input.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        let got = frames(&input).await.expect("skips the bad body");
        assert_eq!(got, vec!["{}".to_string()]);
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_error() {
        let err = frames(b"Content-Length: 5\r\n\r\n{}")
            .await
            .expect_err("EOF before the declared body length");
        assert!(err.starts_with("body read:"), "{err}");
    }

    #[tokio::test]
    async fn a_non_utf8_header_is_an_error() {
        let err = frames(b"X-Note: \xff\r\n\r\n")
            .await
            .expect_err("invalid header encoding");
        assert_eq!(err, "header line was not UTF-8");
    }

    #[tokio::test]
    async fn an_unparseable_content_length_is_skipped() {
        for length in ["invalid", "-1", "184467440737095516160"] {
            let input = format!("Content-Length: {length}\r\n\r\nContent-Length: 2\r\n\r\n{{}}");
            assert_eq!(frames(input.as_bytes()).await.unwrap(), vec!["{}"]);
        }
    }

    #[tokio::test]
    async fn eof_before_the_header_block_ends_preserves_the_existing_tolerance() {
        for input in [b"Content-Length: 2".as_slice(), b"Content-Length: 2\r\n"] {
            assert_eq!(frames(input).await.unwrap(), Vec::<String>::new());
        }
    }

    #[tokio::test]
    async fn utf8_byte_lengths_survive_short_reads() {
        use tokio::io::AsyncWriteExt;

        let body = r#"{"text":"中文🙂"}"#;
        let input = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        // One byte of capacity forces fragmented reads, including inside
        // multibyte characters, without timing-dependent sleeps.
        let (mut writer, reader) = tokio::io::duplex(1);
        let write = tokio::spawn(async move {
            writer.write_all(input.as_bytes()).await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert_eq!(
            read_lsp_frame(&mut reader).await.unwrap().as_deref(),
            Some(body)
        );
        assert_eq!(read_lsp_frame(&mut reader).await.unwrap(), None);
        write.await.unwrap();
    }
}
