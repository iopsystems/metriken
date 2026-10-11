//! A subscription as a response body on tokio (feature `stream-tokio`).

use std::future::Future;
use std::time::SystemTime;

use bytes::Bytes;
use futures_core::Stream;

use super::route::{Pass, StreamSubscription};

impl StreamSubscription {
    /// The subscription's bytes as a stream: the opening, then one frame per
    /// interval until the stream is dropped.
    ///
    /// Each interval it sleeps to the next deadline after `wall_now()`,
    /// reads `now` from `wall_now` again on waking, awaits `fetch(now)` and
    /// yields [`frame_at`](Self::frame_at)`(pass, now)`. An error is yielded
    /// and ends the stream. `keep_alive` is dropped when the stream ends or is
    /// dropped, which
    /// is how a producer ties a registration to the subscription's
    /// lifetime. `wall_now` is `SystemTime::now` outside tests; a test that
    /// steps the wall clock injects its own, since `tokio::time::pause` does
    /// not control `SystemTime`.
    ///
    /// The wide and long layouts have different pass types, so a producer
    /// calls this once per layout.
    pub fn into_stream<P, F, Fut>(
        self,
        fetch: F,
        keep_alive: impl Send + 'static,
        wall_now: impl Fn() -> SystemTime + Send + 'static,
    ) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static
    where
        P: Pass,
        F: FnMut(SystemTime) -> Fut + Send + 'static,
        Fut: Future<Output = Option<P>> + Send,
    {
        struct State<F, K, W> {
            subscription: StreamSubscription,
            fetch: F,
            _keep_alive: K,
            wall_now: W,
            opened: bool,
        }

        let state = State {
            subscription: self,
            fetch,
            _keep_alive: keep_alive,
            wall_now,
            opened: false,
        };

        futures_util::stream::unfold(Some(state), |state| async move {
            let mut state = state?;
            let item = if !state.opened {
                state.opened = true;
                state.subscription.opening()
            } else {
                // This subscription's own timer, on its own boundaries.
                let before = (state.wall_now)();
                let deadline = state.subscription.next_deadline(before);
                tokio::time::sleep(deadline.duration_since(before).unwrap_or_default()).await;
                // Read on waking, before the fetch: `seq` names the
                // boundary woken for, not the time the pass came back.
                let now = (state.wall_now)();
                let pass = (state.fetch)(now).await;
                state.subscription.frame_at(pass.as_ref(), now)
            };
            let next = if item.is_ok() { Some(state) } else { None };
            Some((item, next))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::StreamExt;

    use super::*;
    use crate::stream::{EncodedGroup, PassStamp, StreamRequest};

    fn clock(at: &Arc<AtomicU64>) -> impl Fn() -> SystemTime + Send + 'static {
        let at = Arc::clone(at);
        move || SystemTime::UNIX_EPOCH + Duration::from_nanos(at.load(Ordering::Relaxed))
    }

    fn seqs(body: Vec<u8>) -> (usize, Vec<(u64, usize)>) {
        let mut reader = dendro::replicate::wire::FrameReader::new(std::io::Cursor::new(body))
            .expect("the preamble reads");
        let mut handshakes = 0;
        let mut out = Vec::new();
        while let Some(frame) = reader.next_frame().expect("decodable") {
            match frame {
                dendro::replicate::Frame::Handshake { .. } => handshakes += 1,
                dendro::replicate::Frame::Rows { seq, rows, .. } => out.push((seq, rows.len())),
                _ => {}
            }
        }
        (handshakes, out)
    }

    /// With no pass, every interval still gets an empty frame with the next
    /// `seq`.
    #[tokio::test(start_paused = true)]
    async fn an_interval_without_a_pass_still_gets_an_empty_frame() {
        let now = Arc::new(AtomicU64::new(1_700_000_000_000_000_000));
        let sub = StreamSubscription::new(
            StreamRequest::default(),
            Duration::from_secs(1),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let stream = sub.into_stream(
            |_| async { None::<(PassStamp, Vec<EncodedGroup>)> },
            (),
            clock(&now),
        );
        futures_util::pin_mut!(stream);

        let mut body = Vec::new();
        for step in 0..5 {
            if step > 0 {
                now.fetch_add(1_000_000_000, Ordering::Relaxed);
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .unwrap_or_else(|_| panic!("chunk {step}: no frame within 10 s"))
                .expect("the stream yields")
                .expect("a chunk");
            body.extend_from_slice(&chunk);
        }
        let (handshakes, frames) = seqs(body);
        assert_eq!(handshakes, 1);
        assert_eq!(frames.len(), 4, "{frames:?}");
        assert!(frames.iter().all(|(_, rows)| *rows == 0));
        for pair in frames.windows(2) {
            assert_eq!(pair[1].0, pair[0].0 + 1, "{frames:?}");
        }
    }

    /// The wall clock steps back thirty seconds mid-stream; `seq` keeps
    /// increasing by one.
    #[tokio::test(start_paused = true)]
    async fn a_backwards_clock_step_cannot_lower_seq_on_the_stream() {
        let now = Arc::new(AtomicU64::new(1_700_000_000_000_000_000));
        let sub = StreamSubscription::new(
            StreamRequest::default(),
            Duration::from_secs(1),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let stream = sub.into_stream(
            |_| async { None::<(PassStamp, Vec<EncodedGroup>)> },
            (),
            clock(&now),
        );
        futures_util::pin_mut!(stream);

        let mut body = stream.next().await.unwrap().unwrap().to_vec();
        for step in 0..6 {
            if step == 3 {
                now.fetch_sub(30_000_000_000, Ordering::Relaxed);
            } else {
                now.fetch_add(1_000_000_000, Ordering::Relaxed);
            }
            body.extend_from_slice(&stream.next().await.unwrap().unwrap());
        }
        let (handshakes, frames) = seqs(body);
        assert_eq!(handshakes, 1);
        let seqs: Vec<u64> = frames.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs.len(), 6);
        for pair in seqs.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "{seqs:?}");
        }
    }

    /// `keep_alive` lives as long as the stream.
    #[tokio::test(start_paused = true)]
    async fn keep_alive_is_dropped_with_the_stream() {
        let guard = Arc::new(());
        let sub = StreamSubscription::new(
            StreamRequest::default(),
            Duration::from_secs(1),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let stream = sub.into_stream(
            |_| async { None::<(PassStamp, Vec<EncodedGroup>)> },
            Arc::clone(&guard),
            SystemTime::now,
        );
        assert_eq!(Arc::strong_count(&guard), 2);
        drop(stream);
        assert_eq!(Arc::strong_count(&guard), 1);
    }
}
