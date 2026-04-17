#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuiescencePolicy {
    pub chunk_quiescence_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSignal<'a> {
    Data(&'a [u8]),
    Quiescent,
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkAccumulator {
    policy: QuiescencePolicy,
    buffer: Vec<u8>,
}

impl ChunkAccumulator {
    pub fn new(policy: QuiescencePolicy) -> Self {
        Self {
            policy,
            buffer: Vec::new(),
        }
    }

    pub fn policy(&self) -> QuiescencePolicy {
        self.policy
    }

    pub fn push(&mut self, signal: ReadSignal<'_>) -> Option<Vec<u8>> {
        match signal {
            ReadSignal::Data(bytes) => {
                self.buffer.extend_from_slice(bytes);
                None
            }
            ReadSignal::Quiescent | ReadSignal::Eof => self.flush(),
        }
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn quiescent_signal_closes_the_current_chunk() {
        let mut accumulator = ChunkAccumulator::new(QuiescencePolicy {
            chunk_quiescence_ms: 25,
        });

        assert_eq!(accumulator.push(ReadSignal::Data(b"hello")), None);
        assert_eq!(accumulator.push(ReadSignal::Data(b" world")), None);
        assert_eq!(
            accumulator.push(ReadSignal::Quiescent),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn eof_flushes_a_non_empty_chunk() {
        let mut accumulator = ChunkAccumulator::new(QuiescencePolicy {
            chunk_quiescence_ms: 25,
        });

        assert_eq!(accumulator.push(ReadSignal::Data("🙂".as_bytes())), None);
        assert_eq!(
            accumulator.push(ReadSignal::Eof),
            Some("🙂".as_bytes().to_vec())
        );
        assert_eq!(accumulator.push(ReadSignal::Eof), None);
    }
}
