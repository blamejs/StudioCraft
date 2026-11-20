//! `Info` from the H.264 encoder's and the hardware encoders' packets.

#[cfg(feature = "h264")]
mod h264 {
    use craft_packet::{ConvertError, Info, Kind, PacketPool};
    use craft_video::h264::{Encoder, EncoderConfig, FrameType, Packet, YuvFrame};

    fn packet(pts: i64, dts: i64, keyframe: bool, frame_type: FrameType) -> Packet {
        Packet { data: vec![0, 0, 0, 1, 0x65, 0x88], pts, dts, keyframe, frame_type, qp: 26 }
    }

    #[test]
    fn keeps_timestamps_and_keyframe_flag() {
        let info = Info::try_from(&packet(3000, 0, true, FrameType::Idr)).unwrap();
        assert_eq!(info, Info { kind: Kind::Video, pts: 3000, dts: 0, keyframe: true });
        let info = Info::try_from(&packet(9000, 4500, false, FrameType::P)).unwrap();
        assert_eq!(info, Info { kind: Kind::Video, pts: 9000, dts: 4500, keyframe: false });
        let info = Info::try_from(&packet(4500, 3000, false, FrameType::B)).unwrap();
        assert_eq!((info.pts, info.dts, info.keyframe), (4500, 3000, false));
        // The encoder starts decoding times below 0 when it reorders pictures.
        let info = Info::try_from(&packet(0, -3000, true, FrameType::Idr)).unwrap();
        assert_eq!((info.pts, info.dts), (0, -3000));
        let info = Info::try_from(&packet(i64::MAX, i64::MIN, false, FrameType::I)).unwrap();
        assert_eq!((info.pts, info.dts, info.keyframe), (i64::MAX, i64::MIN, false));
    }

    #[test]
    fn refuses_a_packet_decoded_after_it_is_shown() {
        assert_eq!(Info::try_from(&packet(1000, 1001, false, FrameType::P)), Err(ConvertError::DecodedAfterShown { pts: 1000, dts: 1001 }));
        assert_eq!(
            Info::try_from(&packet(i64::MIN, i64::MAX, true, FrameType::Idr)),
            Err(ConvertError::DecodedAfterShown { pts: i64::MIN, dts: i64::MAX })
        );
    }

    #[test]
    fn refuses_a_keyframe_flag_that_does_not_match_the_picture_type() {
        for frame_type in [FrameType::I, FrameType::P, FrameType::B] {
            assert_eq!(Info::try_from(&packet(0, 0, true, frame_type)), Err(ConvertError::KeyframeMismatch), "{frame_type:?}");
        }
        assert_eq!(Info::try_from(&packet(0, 0, false, FrameType::Idr)), Err(ConvertError::KeyframeMismatch));
    }

    #[test]
    fn refuses_an_empty_packet() {
        let mut p = packet(0, 0, true, FrameType::Idr);
        p.data.clear();
        assert_eq!(Info::try_from(&p), Err(ConvertError::Empty));
    }

    /// The encoder's own packets, with B-frames, convert and share with the timing it gave them.
    #[test]
    fn encoder_packets_share_with_their_timing() {
        let mut cfg = EncoderConfig::new(64, 64, 30, 1);
        cfg.keyint = 4;
        cfg.bframes = 2;
        let mut enc = Encoder::new(cfg).unwrap();
        let (u, v) = (vec![128u8; 32 * 32], vec![128u8; 32 * 32]);
        let mut packets = Vec::new();
        for i in 0..10u8 {
            let y: Vec<u8> = (0..64 * 64).map(|p: usize| (p as u8).wrapping_mul(i.wrapping_add(1))).collect();
            let frame = YuvFrame { y: &y, u: &u, v: &v, y_stride: 64, uv_stride: 32 };
            packets.extend(enc.encode(&frame, i64::from(i) * 3000).unwrap());
        }
        packets.extend(enc.flush());
        assert_eq!(packets.len(), 10);
        assert!(packets.iter().any(|p| p.dts < p.pts), "no picture was reordered");

        let mut pool = PacketPool::new(4, 1024).unwrap();
        let mut keyframes = 0;
        let mut last_dts = i64::MIN;
        for mut p in packets {
            let (pts, dts, keyframe, len) = (p.pts, p.dts, p.keyframe, p.data.len());
            let info = Info::try_from(&p).unwrap();
            let shared = pool.share(&mut p.data, info).unwrap();
            assert_eq!((shared.kind(), shared.pts(), shared.dts(), shared.keyframe(), shared.len()), (Kind::Video, pts, dts, keyframe, len));
            assert!(shared.dts() >= last_dts, "packets are not in decoding order");
            last_dts = shared.dts();
            keyframes += usize::from(shared.keyframe());
        }
        assert!(keyframes >= 1);
    }
}

#[cfg(feature = "hwenc-native")]
mod hwenc_native {
    use craft_hwenc_native::Packet;
    use craft_packet::{ConvertError, Info, Kind, PacketPool};

    fn packet(pts: i64, keyframe: bool) -> Packet {
        Packet { data: vec![0, 0, 0, 2, 0x65, 0x88], pts, keyframe }
    }

    #[test]
    fn decodes_each_picture_when_it_is_shown() {
        assert_eq!(Info::try_from(&packet(9000, true)).unwrap(), Info { kind: Kind::Video, pts: 9000, dts: 9000, keyframe: true });
        assert_eq!(Info::try_from(&packet(12_000, false)).unwrap(), Info { kind: Kind::Video, pts: 12_000, dts: 12_000, keyframe: false });
        assert_eq!(Info::try_from(&packet(-1, false)).unwrap(), Info::video(-1, -1, false));
        assert_eq!(Info::try_from(&packet(i64::MIN, true)).unwrap(), Info::video(i64::MIN, i64::MIN, true));
    }

    #[test]
    fn refuses_an_empty_packet() {
        assert_eq!(Info::try_from(&Packet { data: Vec::new(), pts: 0, keyframe: true }), Err(ConvertError::Empty));
    }

    #[test]
    fn shares_with_its_timing() {
        let mut pool = PacketPool::new(2, 64).unwrap();
        let mut p = packet(3000, true);
        let info = Info::try_from(&p).unwrap();
        let shared = pool.share(&mut p.data, info).unwrap();
        assert_eq!((shared.pts(), shared.dts(), shared.keyframe(), shared.data()), (3000, 3000, true, &[0, 0, 0, 2, 0x65, 0x88][..]));
        assert!(p.data.is_empty());
    }
}
