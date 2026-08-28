use git_scylla_core::{LogLine, Stream};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc::Sender;

/// Longest run of bytes accepted without a terminator before it is emitted
/// anyway.
const MAX_LINE: usize = 64 * 1024;

/// Read one stream, emitting a [`LogLine`] per line. Splits on `\n` or `\r`,
/// so `git fetch`/`git push` progress (carriage-return updates) is not
/// buffered into one line. Empty lines are dropped. Text is decoded lossily,
/// per line.
pub async fn pump<R>(reader: R, stream: Stream, tx: Sender<LogLine>)
where
    R: AsyncRead + Unpin,
{
    let mut r = BufReader::with_capacity(32 * 1024, reader);
    let mut line: Vec<u8> = Vec::with_capacity(256);

    loop {
        // The borrow of `r` must end before `consume`, hence the block.
        let (terminated, consumed, eof) = {
            let buf = match r.fill_buf().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!(%e, ?stream, "child pipe read failed");
                    break;
                }
            };
            if buf.is_empty() {
                (false, 0, true)
            } else {
                match buf.iter().position(|&b| b == b'\n' || b == b'\r') {
                    Some(i) => {
                        line.extend_from_slice(&buf[..i]);
                        (true, i + 1, false)
                    }
                    None => {
                        line.extend_from_slice(buf);
                        (false, buf.len(), false)
                    }
                }
            }
        };
        r.consume(consumed);

        if eof {
            break;
        }
        if (terminated || line.len() >= MAX_LINE) && !emit(&mut line, stream, &tx).await {
            return;
        }
    }

    let _ = emit(&mut line, stream, &tx).await;
}

/// Send `line` if it is non-empty, and clear it. Returns false if the receiver
/// is gone, which means the job finished and there is nothing left to tell.
async fn emit(line: &mut Vec<u8>, stream: Stream, tx: &Sender<LogLine>) -> bool {
    if line.is_empty() {
        return true;
    }
    let text = String::from_utf8_lossy(line).into_owned();
    line.clear();
    tx.send(LogLine::new(stream, text)).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn split(input: &'static [u8]) -> Vec<String> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        pump(input, Stream::Stdout, tx).await;
        let mut out = Vec::new();
        while let Ok(l) = rx.try_recv() {
            out.push(l.text);
        }
        out
    }

    #[tokio::test]
    async fn splits_on_newlines() {
        assert_eq!(split(b"one\ntwo\nthree\n").await, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn a_trailing_line_without_a_terminator_is_kept() {
        assert_eq!(split(b"one\nfatal: no").await, ["one", "fatal: no"]);
    }

    #[tokio::test]
    async fn splits_progress_output_on_carriage_returns() {
        assert_eq!(
            split(b"Receiving objects:  1%\rReceiving objects: 50%\rReceiving objects: 100%\r")
                .await,
            ["Receiving objects:  1%", "Receiving objects: 50%", "Receiving objects: 100%"]
        );
    }

    #[tokio::test]
    async fn crlf_produces_one_line_not_two() {
        assert_eq!(split(b"one\r\ntwo\r\n").await, ["one", "two"]);
    }

    #[tokio::test]
    async fn blank_lines_are_dropped() {
        assert_eq!(split(b"one\n\n\ntwo\n").await, ["one", "two"]);
        assert_eq!(split(b"\n\n").await, Vec::<String>::new());
        assert_eq!(split(b"").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn non_utf8_bytes_become_a_readable_line() {
        let out = split(b"fatal: cannot stat 'bad\xff\xfename'\n").await;
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("fatal: cannot stat 'bad"));
        assert!(out[0].contains('\u{fffd}'), "invalid bytes become replacement chars");
    }

    #[tokio::test]
    async fn a_line_longer_than_the_cap_is_flushed_rather_than_buffered() {
        static HUGE: &[u8] = &[b'x'; MAX_LINE * 2 + 10];
        let out = split(HUGE).await;
        assert!(out.len() >= 2, "expected the oversized line to be broken up, got {}", out.len());
        assert!(out.iter().all(|l| l.len() <= MAX_LINE + 1));
        assert_eq!(out.iter().map(|l| l.len()).sum::<usize>(), HUGE.len());
    }
}
