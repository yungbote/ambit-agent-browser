//! Video for the viewer channel: the codecs a viewer may declare, the encoder
//! boundary, and its one implementation over the image's libaom.
//!
//! The encoder is reached through a hand-written minimal FFI to the shared
//! library the workspace image already ships, opened at run time
//! (`library`), so builds and platforms without it are unaffected: they offer
//! no video and viewers keep JPEG frames.

#[cfg(target_os = "linux")]
mod aom;
#[cfg(all(test, target_os = "linux"))]
mod bench;
#[cfg(target_os = "linux")]
pub(crate) mod convert;
#[cfg(target_os = "linux")]
mod library;
#[cfg(all(test, target_os = "linux"))]
mod webcodecs_e2e;

/// libaom's own decoder, for proofs that decode what the producer sent.
#[cfg(all(test, target_os = "linux"))]
pub(crate) use aom::tests::{Decoded, Decoder};
/// The AV1 encoder itself, for the measurement harnesses: it takes the
/// thread and tile changes the producer does not make.
#[cfg(all(test, target_os = "linux"))]
pub(crate) use aom::{tile_columns_log2, AomEncoder};

/// The closed vocabulary a viewer declares (`video=<token>,...`), in the
/// producer's own order of preference: full chroma first, since text in
/// colour survives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    Av1Full,
    Av1,
    Vp9Full,
    Vp9,
}

impl VideoCodec {
    pub(crate) const ALL: [Self; 4] = [Self::Av1Full, Self::Av1, Self::Vp9Full, Self::Vp9];

    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::Av1Full => "av1-444",
            Self::Av1 => "av1",
            Self::Vp9Full => "vp9-444",
            Self::Vp9 => "vp9",
        }
    }

    pub(crate) fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|codec| codec.token() == token)
    }

    pub(crate) const fn chroma(self) -> Chroma {
        match self {
            Self::Av1Full | Self::Vp9Full => Chroma::Full,
            Self::Av1 | Self::Vp9 => Chroma::Subsampled,
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The codecs a viewer declared (`video=<token>,...`). The producer's order,
/// not the viewer's, chooses among them (contract section 1), so only the
/// set is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Declared(u8);

impl Declared {
    /// Distinct known tokens, or no declaration at all.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let mut codecs = 0;
        for token in value.split(',') {
            let bit = VideoCodec::parse(token)?.bit();
            if codecs & bit != 0 {
                return None;
            }
            codecs |= bit;
        }
        Some(Self(codecs))
    }

    pub(crate) const fn contains(self, codec: VideoCodec) -> bool {
        self.0 & codec.bit() != 0
    }

    /// The codec to offer: the first in the producer's order that the
    /// viewer declared and this process can encode.
    pub(crate) fn negotiate(self) -> Option<VideoCodec> {
        VideoCodec::ALL
            .into_iter()
            .find(|codec| self.contains(*codec) && encodes(*codec))
    }
}

/// How the planes of a picture sample colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chroma {
    /// 4:2:0: chroma at half resolution in both directions.
    Subsampled,
    /// 4:4:4: chroma at full resolution.
    Full,
}

impl Chroma {
    /// Bytes of one plane-set of `width` × `height` with tightly packed rows
    /// (even dimensions for 4:2:0).
    pub(crate) const fn picture_bytes(self, width: usize, height: usize) -> usize {
        match self {
            Self::Subsampled => width * height + 2 * (width / 2) * (height / 2),
            Self::Full => 3 * width * height,
        }
    }
}

/// One picture in planar Y′CbCr (BT.709 at limited range, `convert::COLOUR`),
/// rows tightly packed: Y, then Cb, then Cr.
pub(crate) struct Picture<'a> {
    pub chroma: Chroma,
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

impl<'a> Picture<'a> {
    /// The planes and their strides.
    pub(crate) fn planes(&self) -> Option<[(&'a [u8], usize); 3]> {
        let (width, height) = (self.width as usize, self.height as usize);
        let (chroma_width, chroma_height) = match self.chroma {
            Chroma::Subsampled if width % 2 == 0 && height % 2 == 0 => (width / 2, height / 2),
            Chroma::Subsampled => return None,
            Chroma::Full => (width, height),
        };
        if width == 0 || height == 0 || self.data.len() != self.chroma.picture_bytes(width, height)
        {
            return None;
        }
        let data: &'a [u8] = self.data;
        let (luma, chroma) = data.split_at(width * height);
        let (cb, cr) = chroma.split_at(chroma_width * chroma_height);
        Some([(luma, width), (cb, chroma_width), (cr, chroma_width)])
    }
}

/// What one encode is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EncodeRequest {
    /// A key unit: decodable without references.
    pub key: bool,
    /// The quantizer, 0 (best) to 63.
    pub quantizer: u8,
    /// A refinement of the picture just encoded, unchanged: it happens once
    /// per stop, so it may take more effort than a picture of motion.
    pub refine: bool,
}

/// One temporal unit of one frame.
#[derive(Debug)]
pub(crate) struct EncodedUnit {
    pub data: Vec<u8>,
    pub key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VideoError {
    /// No encoder here: the library is absent or refused this configuration.
    Unavailable(String),
    /// The encoder failed a picture; the stream cannot continue on it.
    Failed(String),
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "video unavailable: {detail}"),
            Self::Failed(detail) => write!(formatter, "video encode failed: {detail}"),
        }
    }
}

/// The path's bitrate, actual picture period and payload target for keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncoderRate {
    pub bits_per_second: u32,
    pub pictures_per_second: u32,
    pub key_bytes: u32,
}

/// One picture in, one temporal unit out, without lookahead or reordering.
pub(crate) trait VideoEncoder: Send {
    /// The encoded picture size.
    fn coded(&self) -> (u32, u32);
    /// The WebCodecs codec string of this stream.
    fn codec_string(&self) -> String;
    /// A path's explicit rate budget; absence preserves fixed-quality encoding.
    fn set_rate(&mut self, rate: Option<EncoderRate>) -> Result<(), VideoError>;
    fn encode(
        &mut self,
        picture: &Picture<'_>,
        request: EncodeRequest,
    ) -> Result<EncodedUnit, VideoError>;
}

/// Whether this process can encode `codec` (its library is present and of
/// the expected interface).
pub(crate) fn encodes(codec: VideoCodec) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(codec, VideoCodec::Av1 | VideoCodec::Av1Full) && aom::available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = codec;
        false
    }
}

/// An encoder for `codec` at `width` × `height` running `threads` threads.
pub(crate) fn open(
    codec: VideoCodec,
    width: u32,
    height: u32,
    threads: u32,
) -> Result<Box<dyn VideoEncoder>, VideoError> {
    #[cfg(target_os = "linux")]
    {
        match codec {
            VideoCodec::Av1 | VideoCodec::Av1Full => Ok(Box::new(aom::AomEncoder::new(
                codec, width, height, threads,
            )?)),
            VideoCodec::Vp9 | VideoCodec::Vp9Full => Err(VideoError::Unavailable(
                "VP9 has no encoder in this build".into(),
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (codec, width, height, threads);
        Err(VideoError::Unavailable(
            "no video encoder on this platform".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_is_distinct_known_tokens_or_nothing() {
        let declared = Declared::parse("vp9,av1,av1-444").unwrap();
        for (codec, listed) in [
            (VideoCodec::Av1Full, true),
            (VideoCodec::Av1, true),
            (VideoCodec::Vp9Full, false),
            (VideoCodec::Vp9, true),
        ] {
            assert_eq!(declared.contains(codec), listed, "{codec:?}");
        }
        for invalid in ["", "h264", "av1,av1", "av1,,vp9", "AV1", "av1 ,vp9"] {
            assert_eq!(Declared::parse(invalid), None, "{invalid}");
        }
        for codec in VideoCodec::ALL {
            assert_eq!(VideoCodec::parse(codec.token()), Some(codec));
        }
    }

    /// The producer's order decides, and only a codec it can encode is
    /// offered: VP9 never is, whatever the viewer prefers.
    #[test]
    fn negotiation_follows_the_producers_order_among_what_it_encodes() {
        let aom = encodes(VideoCodec::Av1);
        assert_eq!(aom, encodes(VideoCodec::Av1Full));
        let offer = |value: &str| Declared::parse(value).unwrap().negotiate();
        assert_eq!(offer("av1,av1-444"), aom.then_some(VideoCodec::Av1Full));
        assert_eq!(offer("vp9,av1"), aom.then_some(VideoCodec::Av1));
        assert_eq!(offer("vp9-444,vp9"), None);
    }

    #[test]
    fn a_picture_names_its_planes_only_when_its_bytes_fit_exactly() {
        let data = vec![0u8; Chroma::Subsampled.picture_bytes(4, 2)];
        let picture = Picture {
            chroma: Chroma::Subsampled,
            width: 4,
            height: 2,
            data: &data,
        };
        let planes = picture.planes().unwrap();
        assert_eq!(
            planes.map(|(plane, stride)| (plane.len(), stride)),
            [(8, 4), (2, 2), (2, 2)]
        );
        let full = vec![0u8; Chroma::Full.picture_bytes(4, 2)];
        let picture = Picture {
            chroma: Chroma::Full,
            width: 4,
            height: 2,
            data: &full,
        };
        assert_eq!(
            picture
                .planes()
                .unwrap()
                .map(|(plane, stride)| (plane.len(), stride)),
            [(8, 4), (8, 4), (8, 4)]
        );
        for (chroma, width, height, bytes) in [
            (Chroma::Subsampled, 3, 2, 12),
            (Chroma::Subsampled, 4, 2, 11),
            (Chroma::Full, 0, 2, 0),
        ] {
            let data = vec![0u8; bytes];
            let picture = Picture {
                chroma,
                width,
                height,
                data: &data,
            };
            assert!(picture.planes().is_none(), "{chroma:?} {width}x{height}");
        }
    }
}
