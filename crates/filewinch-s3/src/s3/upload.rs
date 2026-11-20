//! Uploads: one streamed PUT for known sizes under 16 MiB, multipart otherwise.
//!
//! Memory stays bounded: a known-size part is streamed through [`upload_pipe`] with its exact
//! Content-Length (about 1 MiB in flight); an unknown-size upload buffers one part at a time
//! (it must know each part's length before sending it), and if the data ends before the
//! first part fills, it becomes a single PUT.

use std::io::{self, Write};
use std::sync::Arc;

use filewinch_core::{Error, ErrorKind, Result, WriteSink};
use filewinch_http::{UploadPipe, to_io, upload_pipe};

use super::client::{Call, Client};
use super::multipart::{Multipart, etag, part_size_for, part_size_unknown};

pub(crate) struct S3Sink {
    client: Arc<Client>,
    bucket: String,
    key: String,
    size: Option<u64>,
    written: u64,
    /// Single PUT: the last byte, sent only by `finish`.
    held: Option<u8>,
    state: State,
    what: String,
}

enum State {
    /// Known size below the multipart threshold; the PUT starts on the first write.
    Single(Option<UploadPipe<()>>),
    /// Unknown size, first part still filling.
    Buffer(Vec<u8>),
    Multi { mp: Multipart, part: Option<Part>, part_size: u64 },
    Done,
}

enum Part {
    /// Known size: streaming, `left` bytes to go.
    Stream { pipe: UploadPipe<String>, left: u64 },
    /// Unknown size: buffered until full.
    Buffer(Vec<u8>),
}

impl S3Sink {
    pub fn start(client: Arc<Client>, bucket: &str, key: &str, size: Option<u64>, what: String) -> Result<S3Sink> {
        client.check_open()?;
        let state = match size {
            Some(n) if n < client.limits.single_put_below => {
                client.ensure_region(bucket);
                State::Single(None)
            }
            Some(n) => {
                let part_size = part_size_for(n, client.limits.part_size, client.limits.max_parts);
                let mp = Multipart::create(client.clone(), bucket, key, &what)?;
                State::Multi { mp, part: None, part_size }
            }
            None => State::Buffer(Vec::new()),
        };
        Ok(S3Sink { client, bucket: bucket.into(), key: key.into(), size, written: 0, held: None, state, what })
    }

    /// Accepts up to `data.len()` bytes; returns how many were taken (> 0 for non-empty data).
    fn accept(&mut self, data: &[u8]) -> Result<usize> {
        let part_min = self.client.limits.part_size;
        let max_parts = self.client.limits.max_parts;
        loop {
            match &mut self.state {
                State::Done => return Err(Error::new(ErrorKind::Other, "The upload has already finished.")),
                State::Single(pipe) => {
                    let len = self.size.unwrap_or(0);
                    if pipe.is_none() {
                        *pipe = Some(single_pipe(&self.client, &self.bucket, &self.key, len, &self.what));
                    }
                    let pipe = pipe.as_mut().expect("just set");
                    // Hold back the final byte until `finish`: with the body one byte short,
                    // dropping the sink aborts the PUT and S3 never commits the object.
                    if self.written + 1 == len {
                        self.held = Some(data[0]);
                        return Ok(1);
                    }
                    let room = (len - 1 - self.written) as usize;
                    return pipe.write(&data[..data.len().min(room)]).map_err(filewinch_http::from_io);
                }
                State::Buffer(buf) => {
                    let room = part_min as usize - buf.len();
                    if room > 0 {
                        let n = room.min(data.len());
                        buf.extend_from_slice(&data[..n]);
                        return Ok(n);
                    }
                    // The first part is full and more data is coming: go multipart.
                    let first = std::mem::take(buf);
                    let mut mp = Multipart::create(self.client.clone(), &self.bucket, &self.key, &self.what)?;
                    let uploaded = mp.upload_bytes(&first, &self.what);
                    let state = State::Multi { mp, part: None, part_size: part_min };
                    self.state = state;
                    uploaded?;
                }
                State::Multi { mp, part, part_size } => match part {
                    None => {
                        let n = mp.next_part();
                        if u64::from(n) > max_parts {
                            return Err(too_many_parts(&self.what));
                        }
                        *part = Some(match self.size {
                            Some(total) => {
                                let done = u64::from(n - 1) * *part_size;
                                let len = (*part_size).min(total.saturating_sub(done));
                                Part::Stream { pipe: part_pipe(&self.client, mp, len, &self.what), left: len }
                            }
                            None => Part::Buffer(Vec::with_capacity(part_size_unknown(n, part_min) as usize)),
                        });
                    }
                    Some(Part::Stream { pipe, left }) => {
                        if *left == 0 {
                            let Some(Part::Stream { pipe, .. }) = part.take() else { unreachable!() };
                            let tag = pipe.finish()?;
                            mp.parts.push((mp.next_part(), tag));
                            continue;
                        }
                        let n = pipe.write(&data[..data.len().min(*left as usize)]).map_err(filewinch_http::from_io)?;
                        *left -= n as u64;
                        return Ok(n);
                    }
                    Some(Part::Buffer(buf)) => {
                        let target = part_size_unknown(mp.next_part(), part_min) as usize;
                        if buf.len() < target {
                            let n = (target - buf.len()).min(data.len());
                            buf.extend_from_slice(&data[..n]);
                            return Ok(n);
                        }
                        let full = std::mem::take(buf);
                        *part = None;
                        mp.upload_bytes(&full, &self.what)?;
                    }
                },
            }
        }
    }

    fn finish_inner(&mut self) -> Result<()> {
        if self.size.is_some_and(|total| self.written != total) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("Couldn't {}: the file changed size while it was being uploaded.", self.what),
            ));
        }
        match std::mem::replace(&mut self.state, State::Done) {
            State::Done => Ok(()),
            State::Single(Some(mut pipe)) => {
                if let Some(last) = self.held.take() {
                    pipe.write_all(&[last]).map_err(filewinch_http::from_io)?;
                }
                pipe.finish()
            }
            State::Single(None) => self.put_bytes(&[]),
            State::Buffer(buf) => self.put_bytes(&buf),
            State::Multi { mut mp, part, .. } => {
                let result = (|| {
                    match part {
                        Some(Part::Stream { pipe, .. }) => {
                            let tag = pipe.finish()?;
                            mp.parts.push((mp.next_part(), tag));
                        }
                        Some(Part::Buffer(buf)) if !buf.is_empty() => mp.upload_bytes(&buf, &self.what)?,
                        _ => {}
                    }
                    if mp.parts.is_empty() {
                        // A zero-byte multipart upload still needs one (empty) part.
                        mp.upload_bytes(&[], &self.what)?;
                    }
                    mp.complete(&self.what)
                })();
                if result.is_err() {
                    mp.abort_in_background();
                }
                result
            }
        }
    }

    fn put_bytes(&self, data: &[u8]) -> Result<()> {
        self.client.send(&Call::new("PUT", Some(&self.bucket), &self.key).body(data), &self.what).map(drop)
    }
}

fn single_pipe(client: &Arc<Client>, bucket: &str, key: &str, len: u64, what: &str) -> UploadPipe<()> {
    let (client, bucket, key, what) = (client.clone(), bucket.to_string(), key.to_string(), what.to_string());
    upload_pipe(move |reader| client.put_stream(&Call::new("PUT", Some(&bucket), &key), reader, len, &what).map(drop))
}

fn part_pipe(client: &Arc<Client>, mp: &Multipart, len: u64, what: &str) -> UploadPipe<String> {
    let (client, n, what) = (client.clone(), mp.next_part(), what.to_string());
    let (bucket, key, id) = (mp.bucket.clone(), mp.key.clone(), mp.upload_id.clone());
    upload_pipe(move |reader| {
        let call = Call::new("PUT", Some(&bucket), &key)
            .query("partNumber", n.to_string())
            .query("uploadId", id);
        let resp = client.put_stream(&call, reader, len, &what)?;
        etag(&resp, &client, &what)
    })
}

fn too_many_parts(what: &str) -> Error {
    Error::new(
        ErrorKind::NoSpace,
        format!("Couldn't {what}: the file is too large to upload without knowing its size first."),
    )
}

impl Write for S3Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        if self.size.is_some_and(|total| self.written + data.len() as u64 > total) {
            return Err(to_io(Error::new(
                ErrorKind::Other,
                format!("Couldn't {}: the file grew while it was being uploaded.", self.what),
            )));
        }
        let n = self.accept(data).map_err(to_io)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteSink for S3Sink {
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.finish_inner()
    }
}

impl Drop for S3Sink {
    fn drop(&mut self) {
        // Not finished: drop any streaming part (its request aborts mid-body) and abort the
        // multipart upload so nothing is committed.
        if let State::Multi { mp, .. } = std::mem::replace(&mut self.state, State::Done) {
            mp.abort_in_background();
        }
    }
}
