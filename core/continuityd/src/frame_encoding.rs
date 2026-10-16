//! Turning a captured screen into what a remote-control viewer is sent —
//! the limits every host keeps to, and the RGB scaling and JPEG encoding the
//! Linux and Windows hosts share (macOS draws and encodes through Core
//! Graphics and AppKit, see `remote_control_mac.rs`, to the same limits).

/// Frames a second a host captures, at most. **Real complaint fixed here**:
/// it was 10, and since the other computer's pointer is part of the
/// picture, the pointer moved ten times a second too — choppy enough to
/// make desktop-to-desktop control "barely usable".
pub const FRAMES_PER_SECOND: u64 = 30;

/// Frames wider than this are scaled down by a whole factor before
/// encoding (3840 → 1920, 2880 → 1440, 5120 → 1707): a viewer rarely shows
/// more across, and encoding — and decoding — a 4K frame thirty times a
/// second would keep a core busy on both sides.
pub const MAX_ENCODED_WIDTH: usize = 1920;

/// JPEG quality, out of 100. It was 40, which left text blurry; with frames
/// no wider than `MAX_ENCODED_WIDTH` and unchanged ones not sent at all
/// (`LastFrame`), 60 is affordable on a LAN.
pub const JPEG_QUALITY: u8 = 60;

/// The last frame actually handed on, so an identical one isn't sent again:
/// a still screen costs nothing on the network or in the viewer's decoder.
/// Only a frame that was really sent counts — one dropped because the
/// stream was full must still go out, or the viewer would keep the frame
/// before it for as long as nothing changed.
#[derive(Default)]
pub struct LastFrame(Option<Vec<u8>>);

impl LastFrame {
    /// Whether `frame` is new — differs from the last one sent.
    pub fn is_new(&self, frame: &[u8]) -> bool {
        self.0.as_deref() != Some(frame)
    }

    /// Records `frame` as sent.
    pub fn sent(&mut self, frame: Vec<u8>) {
        self.0 = Some(frame);
    }
}

/// Picks the R, G and B bytes (at `channels`) out of each 4-byte pixel of a
/// `width` × `height` frame whose rows are `stride` bytes apart (0 or less:
/// packed), sampling every `factor`th pixel and row when the frame is wider
/// than `MAX_ENCODED_WIDTH`. A buffer shorter than the frame it claims to
/// hold is skipped rather than drawn half-finished.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn to_rgb(pixels: &[u8], width: usize, height: usize, stride: i32, channels: [usize; 3]) -> Option<(Vec<u8>, usize, usize)> {
    let row_bytes = width.checked_mul(4)?;
    let stride = if stride > 0 { stride as usize } else { row_bytes };
    if stride < row_bytes {
        return None;
    }
    let factor = width.div_ceil(MAX_ENCODED_WIDTH).max(1);
    let (out_width, out_height) = (width / factor, height / factor);
    if out_width == 0 || out_height == 0 {
        return None;
    }
    let mut rgb = Vec::with_capacity(out_width * out_height * 3);
    for row in 0..out_height {
        let start = row * factor * stride;
        let line = pixels.get(start..start + out_width * factor * 4)?;
        for pixel in line.chunks_exact(4 * factor) {
            rgb.extend_from_slice(&[pixel[channels[0]], pixel[channels[1]], pixel[channels[2]]]);
        }
    }
    Some((rgb, out_width, out_height))
}

/// Packed RGB to JPEG at `JPEG_QUALITY`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn encode_jpeg(rgb: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    let mut jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    if let Err(e) = encoder.encode(rgb, width as u32, height as u32, image::ExtendedColorType::Rgb8) {
        tracing::debug!("JPEG encode failed: {e}");
        return None;
    }
    Some(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGR: [usize; 3] = [2, 1, 0];

    #[test]
    fn to_rgb_reorders_channels_and_skips_row_padding() {
        // 2x2 BGRx frame, each row padded to 12 bytes.
        let pixels = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let (rgb, width, height) = to_rgb(&pixels, 2, 2, 12, BGR).expect("converts");
        assert_eq!((width, height), (2, 2));
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }
    #[test]
    fn to_rgb_downscales_frames_wider_than_the_limit() {
        let (width, height) = (MAX_ENCODED_WIDTH * 2, 4);
        let pixels = vec![9u8; width * height * 4];
        let (rgb, out_width, out_height) = to_rgb(&pixels, width, height, 0, BGR).expect("converts");
        assert_eq!((out_width, out_height), (MAX_ENCODED_WIDTH, 2));
        assert_eq!(rgb.len(), out_width * out_height * 3);
    }

    #[test]
    fn to_rgb_skips_a_buffer_shorter_than_its_frame() {
        let pixels = vec![0u8; 2 * 2 * 4 - 1];
        assert!(to_rgb(&pixels, 2, 2, 8, BGR).is_none());
    }

    #[test]
    fn only_a_frame_that_differs_from_the_last_one_sent_is_new() {
        let mut last = LastFrame::default();
        assert!(last.is_new(b"first"));
        last.sent(b"first".to_vec());
        assert!(!last.is_new(b"first"), "a still screen sends nothing more");
        assert!(last.is_new(b"second"));
    }

    #[test]
    fn packed_rgb_encodes_to_a_jpeg() {
        let jpeg = encode_jpeg(&vec![128u8; 16 * 8 * 3], 16, 8).expect("encodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "a JPEG starts with SOI");
    }
}
