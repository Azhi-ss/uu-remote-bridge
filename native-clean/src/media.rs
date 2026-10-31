//! H.264 RTP adaptation for the UU Windows encoder.
use bytes::Bytes;
use webrtc::rtp::{codecs::h264::H264Packet, packetizer::Depacketizer};

#[derive(Default)]
pub struct H264Depacketizer(H264Packet);

impl Depacketizer for H264Depacketizer {
    fn depacketize(&mut self, payload: &Bytes) -> Result<Bytes, webrtc::rtp::Error> {
        // UU sends a two-byte access-unit delimiter as its own RTP packet.
        // rtp 0.10 rejects payloads <= 2 bytes, causing SampleBuilder to drop
        // the entire following reference frame. Padding is not video data.
        if payload.is_empty() {
            return Ok(Bytes::new());
        }
        if matches!(payload[0] & 31, 1..=23) {
            let mut nal = Vec::with_capacity(payload.len() + 4);
            nal.extend_from_slice(&[0, 0, 0, 1]);
            nal.extend_from_slice(payload);
            return Ok(Bytes::from(nal));
        }
        self.0.depacketize(payload)
    }

    fn is_partition_head(&self, payload: &Bytes) -> bool {
        payload.is_empty()
            || payload.first().is_some_and(|b| matches!(b & 31, 1..=23))
            || self.0.is_partition_head(payload)
    }

    fn is_partition_tail(&self, marker: bool, payload: &Bytes) -> bool {
        self.0.is_partition_tail(marker, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::{
        media::io::sample_builder::SampleBuilder,
        rtp::{header::Header, packet::Packet},
    };

    #[test]
    fn short_aud_preserves_the_reference_frame() {
        let mut builder = SampleBuilder::new(2048, H264Depacketizer::default(), 90_000);
        for (seq, ts, marker, payload) in [
            (1, 3000, false, vec![0x09, 0xf0]),
            (2, 3000, true, vec![0x61, 0x11, 0x22]),
            (3, 6000, false, vec![0x09, 0xf0]),
        ] {
            builder.push(Packet {
                header: Header {
                    sequence_number: seq,
                    timestamp: ts,
                    marker,
                    ..Default::default()
                },
                payload: Bytes::from(payload),
            });
        }
        let sample = builder
            .pop()
            .expect("AUD must not discard the following P-frame");
        assert_eq!(
            &sample.data[..],
            &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x61, 0x11, 0x22]
        );
        assert_eq!(sample.prev_dropped_packets, 0);
    }

    #[test]
    fn missing_packet_does_not_block_the_next_complete_frame() {
        let mut builder = SampleBuilder::new(2048, H264Depacketizer::default(), 90_000)
            .with_max_time_delay(std::time::Duration::from_millis(500));
        let mut samples = Vec::new();
        for (seq, ts, marker, payload) in [
            (1, 3000, false, vec![0x09, 0xf0]),
            // Sequence 2 was lost; do not wait for thousands more packets.
            (3, 3000, true, vec![0x61, 0x11, 0x22]),
            (4, 60000, false, vec![0x09, 0xf0]),
            (5, 60000, true, vec![0x65, 0x33, 0x44]),
            (6, 63000, false, vec![0x09, 0xf0]),
        ] {
            builder.push(Packet {
                header: Header {
                    sequence_number: seq,
                    timestamp: ts,
                    marker,
                    ..Default::default()
                },
                payload: Bytes::from(payload),
            });
            while let Some(sample) = builder.pop() {
                samples.push(sample);
            }
        }
        assert!(samples
            .iter()
            .any(|sample| sample.packet_timestamp == 60000
                && sample.data.ends_with(&[0x65, 0x33, 0x44])));
    }
    #[test]
    fn padding_and_fragmented_idr_are_supported() {
        let mut dep = H264Depacketizer::default();
        assert!(dep.depacketize(&Bytes::new()).unwrap().is_empty());
        assert!(dep
            .depacketize(&Bytes::from_static(&[0x7c, 0x85, 1, 2]))
            .unwrap()
            .is_empty());
        assert_eq!(
            &dep.depacketize(&Bytes::from_static(&[0x7c, 0x45, 3, 4]))
                .unwrap()[..],
            &[0, 0, 0, 1, 0x65, 1, 2, 3, 4]
        );
    }
}
