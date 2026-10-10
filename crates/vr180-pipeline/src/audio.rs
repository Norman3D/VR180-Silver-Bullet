//! Audio extraction + spatial-audio plumbing.
//!
//! Phase 0.8.6 adds the side of the pipeline that handles the GoPro
//! MAX 2's ambisonic 4-channel PCM track (stream index 5 in the `.360`
//! container; `pcm_s24le`, 48 kHz, AmbiX W,Y,Z,X channel layout).
//!
//! Pipeline:
//!   .360 → `extract_ambisonic_to_wav` (in-process libav remux)
//!        → temp.wav (4ch pcm_s24le)
//!        → `helpers::spawn_apac_encode` (Swift helper, Vision Pro APAC)
//!        → final.mov (video passthrough + APAC audio)
//!
//! In-process WAV remux avoids the Python app's subprocess hop to the
//! system ffmpeg binary; ffmpeg-next opens the `.360` once for video
//! decode and a second time here for audio extraction (the cost is
//! ~1ms of header parsing per 5+ GB file — negligible).

use crate::{Error, Result};
use ffmpeg_next as ffmpeg;
use std::path::Path;

/// Description of the source's ambisonic audio track.
#[derive(Debug, Clone, Copy)]
pub struct AmbisonicInfo {
    /// 0-based stream index in the source container.
    pub stream_index: usize,
    pub sample_rate: u32,
    pub channels: u32,
    /// AVCodecID enum value (e.g. `AV_CODEC_ID_PCM_S24LE = 65557`).
    /// Stored as `i32` to avoid leaking ffmpeg-next's `codec::Id` into
    /// public callers that don't depend on it.
    pub codec_id: i32,
}

/// Probe the source `.360` (or any container) for a 4-channel ambisonic
/// audio track. Returns `Ok(Some(_))` if one is found, `Ok(None)` if
/// the file has audio but none ambisonic, or `Err` on open failure.
///
/// GoPro MAX 2 puts the ambisonic track at stream index 5 with codec
/// `pcm_s24le`, 4 channels, "ambisonic 1" layout. We match on the
/// channel count + audio media type + raw PCM codec ID to be robust
/// against future firmware tweaks (e.g. if GoPro ever switches to
/// pcm_f32le, the match still hits as long as channels == 4 and it's
/// uncompressed PCM).
pub fn probe_ambisonic(input: &Path) -> Result<Option<AmbisonicInfo>> {
    crate::decode::init();
    let ictx = open_input_concat_aware(input)
        .map_err(|e| Error::Ffmpeg(format!("open {input:?}: {e}")))?;
    for stream in ictx.streams() {
        let params = stream.parameters();
        let medium = params.medium();
        if medium != ffmpeg::media::Type::Audio { continue; }
        // SAFETY: we hold the input context borrow via `stream`.
        let (channels, codec_id, sample_rate) = unsafe {
            let raw = params.as_ptr();
            (
                (*raw).ch_layout.nb_channels as u32,
                (*raw).codec_id as i32,
                (*raw).sample_rate as u32,
            )
        };
        if channels != 4 { continue; }
        // Accept any uncompressed PCM ambisonic — pcm_s24le is what
        // GoPro produces today; pcm_s16le / pcm_f32le would be valid
        // if the firmware ever switches.
        if !is_pcm_codec_id(codec_id) { continue; }
        return Ok(Some(AmbisonicInfo {
            stream_index: stream.index(),
            sample_rate,
            channels,
            codec_id,
        }));
    }
    Ok(None)
}

/// The Insta360 X6's "360° Audio" track: a 4-channel AAC with no channel
/// semantics in the file (AAC channel config 4, no `chan`/SA3D). Measured
/// against Insta360 Studio's SA3D-tagged export (2026-10-10): the decoded
/// channels ARE first-order AmbiX — `[W, Y, Z, X]` (ACN order, SN3D) — Studio
/// only rotates X/Y by its stabilisation yaw, scales, and tags SA3D.
/// (Older firmware / the "Auto" or "Stereo" audio modes write stereo AAC,
/// for which this returns `None`.)
pub fn probe_foa_aac(input: &Path) -> Result<Option<AmbisonicInfo>> {
    crate::decode::init();
    let ictx = open_input_concat_aware(input)
        .map_err(|e| Error::Ffmpeg(format!("open {input:?}: {e}")))?;
    for stream in ictx.streams() {
        let params = stream.parameters();
        if params.medium() != ffmpeg::media::Type::Audio { continue; }
        // SAFETY: we hold the input context borrow via `stream`.
        let (channels, codec_id, sample_rate) = unsafe {
            let raw = params.as_ptr();
            ((*raw).ch_layout.nb_channels as u32, (*raw).codec_id as i32, (*raw).sample_rate as u32)
        };
        if channels != 4 || codec_id != ffmpeg::ffi::AVCodecID::AV_CODEC_ID_AAC as i32 { continue; }
        return Ok(Some(AmbisonicInfo { stream_index: stream.index(), sample_rate, channels, codec_id }));
    }
    Ok(None)
}

/// Any 4-channel first-order ambisonic track: GoPro's PCM
/// (`probe_ambisonic`) or the X6's AAC (`probe_foa_aac`).
pub fn probe_spatial_4ch(input: &Path) -> Result<Option<AmbisonicInfo>> {
    match probe_ambisonic(input)? {
        Some(i) => Ok(Some(i)),
        None => probe_foa_aac(input),
    }
}

/// Front-facing stereo decode of first-order AmbiX: a pair of virtual
/// cardioids at ±45° — SN3D cardioid = ½·W + ½·(X·cos θ + Y·sin θ), +X the
/// camera's front (its front lens, the VR180 rig's forward), +Y left.
/// Rows `[L, R]`, columns `[W, Y, Z, X]` (decoded channel order).
/// A plain layout downmix of the X6 track instead cancels two anti-phase
/// channels and lands ~9 dB left-heavy (measured 2026-10-10).
pub const FOA_STEREO_MATRIX: [[f32; 4]; 2] = [
    [0.5,  0.353_553_4, 0.0, 0.353_553_4],
    [0.5, -0.353_553_4, 0.0, 0.353_553_4],
];

/// One sample of `[W, Y, Z, X]` → `[L, R]` through [`FOA_STEREO_MATRIX`].
#[inline]
pub fn foa_to_stereo_sample(wyzx: [f32; 4]) -> [f32; 2] {
    let m = &FOA_STEREO_MATRIX;
    [
        m[0][0] * wyzx[0] + m[0][1] * wyzx[1] + m[0][2] * wyzx[2] + m[0][3] * wyzx[3],
        m[1][0] * wyzx[0] + m[1][1] * wyzx[1] + m[1][2] * wyzx[2] + m[1][3] * wyzx[3],
    ]
}

/// Codec-ID set we recognize as "uncompressed PCM suitable for
/// ambisonic extraction". Extracted to its own function so the list of
/// fallback codecs is documented in one place.
fn is_pcm_codec_id(codec_id: i32) -> bool {
    use ffmpeg::ffi::AVCodecID::*;
    let id = unsafe { std::mem::transmute::<i32, ffmpeg::ffi::AVCodecID>(codec_id) };
    matches!(id,
          AV_CODEC_ID_PCM_S24LE
        | AV_CODEC_ID_PCM_S16LE
        | AV_CODEC_ID_PCM_S32LE
        | AV_CODEC_ID_PCM_F32LE
    )
}

/// Extract the ambisonic audio track from `input` into a fresh WAV
/// file at `output_wav`. Stream-copy remux (no decode / re-encode):
/// the raw PCM packets land in the WAV's `data` chunk byte-for-byte.
///
/// Caller should run `probe_ambisonic` first to confirm a track exists
/// AND to choose between APAC vs stereo-AAC paths. This function
/// errors if the source has no ambisonic stream.
pub fn extract_ambisonic_to_wav(input: &Path, output_wav: &Path) -> Result<AmbisonicInfo> {
    extract_ambisonic_to_wav_window(input, output_wav, 0.0, None)
}

/// Window-aware ambisonic extraction: only the `[offset_s, offset_s +
/// dur_s)` slice of the source's 4-ch track is written (rebased to start
/// at 0), so a TRIMMED export's APAC/ambisonic audio matches the
/// exported video. `offset_s = 0, dur_s = None` extracts the whole track.
pub fn extract_ambisonic_to_wav_window(
    input: &Path,
    output_wav: &Path,
    offset_s: f64,
    dur_s: Option<f64>,
) -> Result<AmbisonicInfo> {
    crate::decode::init();
    let info = probe_spatial_4ch(input)?
        .ok_or_else(|| Error::Ffmpeg(format!(
            "no ambisonic audio track in {input:?}"
        )))?;
    if !is_pcm_codec_id(info.codec_id) {
        // Compressed (the X6's AAC): decode to 24-bit PCM — the packet copy
        // below is PCM-only.
        let n = transcode_audio_window(
            input, info.stream_index, output_wav, offset_s, dur_s, None,
            AudioOut::Pcm24 { channels: info.channels as i32 },
        )?;
        tracing::info!(wav = %output_wav.display(), samples = n, channels = info.channels,
            sample_rate = info.sample_rate, "decoded 4-ch AAC ambisonic audio to WAV");
        return Ok(info);
    }

    let mut ictx = open_input_concat_aware(input)
        .map_err(|e| Error::Ffmpeg(format!("open input: {e}")))?;
    if offset_s > 0.001 {
        let ts = (offset_s * 1_000_000.0) as i64;
        let _ = ictx.seek(ts, ..ts);
    }
    let mut octx = ffmpeg::format::output(&output_wav)
        .map_err(|e| Error::Ffmpeg(format!("open output WAV {output_wav:?}: {e}")))?;

    // Build the output audio stream as a parameter copy of the input.
    let src_params = ictx.stream(info.stream_index)
        .ok_or_else(|| Error::Ffmpeg("ambisonic stream vanished".into()))?
        .parameters();
    let src_time_base = ictx.stream(info.stream_index).unwrap().time_base();

    let out_stream_index = {
        let mut out_stream = octx.add_stream(ffmpeg::codec::Id::None)
            .map_err(|e| Error::Ffmpeg(format!("add_stream: {e}")))?;
        out_stream.set_parameters(src_params);
        // The wav muxer expects an `audio` AVCodecParameters with the
        // PCM codec_id populated, which set_parameters() copied verbatim
        // from the input. Time base mirrors the source so PTS values
        // remain meaningful.
        out_stream.set_time_base(src_time_base);
        // Force `codec_tag = 0` so ffmpeg picks the right wav format
        // (the source MOV uses MOV-specific tags that would confuse
        // the wav muxer's "tag → format" round-trip).
        unsafe {
            let raw = out_stream.parameters().as_mut_ptr();
            (*raw).codec_tag = 0;
        }
        out_stream.index()
    };

    octx.write_header()
        .map_err(|e| Error::Ffmpeg(format!("WAV write_header: {e}")))?;

    // Window bounds in source seconds (None dur → to EOF).
    let tb_s = src_time_base.numerator() as f64 / src_time_base.denominator().max(1) as f64;
    let win_end = dur_s.map(|d| offset_s + d);
    let mut packet_count = 0u64;
    for (stream, mut packet) in ictx.packets() {
        if stream.index() != info.stream_index { continue; }
        // Window by the packet's source time. (Seek lands on/near the
        // keyframe before `offset`; skip the run-in, stop past the end.)
        let t = packet.pts().or(packet.dts()).unwrap_or(0) as f64 * tb_s;
        if t < offset_s - 0.05 { continue; }
        if let Some(end) = win_end { if t >= end { break; } }
        packet.set_stream(out_stream_index);
        // The wav muxer derives its own packet timing from sample count;
        // we just need PTS to monotonically increase. Rescale from
        // source time-base into output's (we set output to match input
        // above, so this is a noop in practice — but keeping it
        // explicit guards against future muxer surprise).
        packet.rescale_ts(
            src_time_base,
            octx.stream(out_stream_index).unwrap().time_base(),
        );
        packet.write_interleaved(&mut octx)
            .map_err(|e| Error::Ffmpeg(format!("write audio packet: {e}")))?;
        packet_count += 1;
    }
    octx.write_trailer()
        .map_err(|e| Error::Ffmpeg(format!("WAV write_trailer: {e}")))?;

    tracing::info!(
        wav = %output_wav.display(),
        packets = packet_count,
        channels = info.channels,
        sample_rate = info.sample_rate,
        "extracted ambisonic audio to WAV"
    );
    Ok(info)
}

/// Output of [`transcode_audio_window`].
enum AudioOut {
    /// Stereo AAC-LC (ffmpeg's native encoder) in an `.m4a`.
    AacStereo { bit_rate: usize },
    /// 24-bit PCM, `channels` wide, in a `.wav`.
    Pcm24 { channels: i32 },
}

/// Decode the `[offset_s, offset_s + dur_s)` window of audio stream
/// `stream_index` (packet granularity at the edges, like the passthrough
/// mux), optionally mix it 4→2 through `mix` (rows = output channels), and
/// encode it to `out_path`. Returns the number of samples written.
fn transcode_audio_window(
    input: &Path,
    stream_index: usize,
    out_path: &Path,
    offset_s: f64,
    dur_s: Option<f64>,
    mix: Option<&[[f32; 4]; 2]>,
    out: AudioOut,
) -> Result<u64> {
    use ffmpeg::format::sample::{Sample, Type};
    use ffmpeg::ChannelLayout;
    crate::decode::init();

    let mut ictx = open_input_concat_aware(input)
        .map_err(|e| Error::Ffmpeg(format!("open input {input:?}: {e}")))?;
    if offset_s > 0.001 {
        let ts = (offset_s * 1_000_000.0) as i64;
        let _ = ictx.seek(ts, ..ts);
    }
    let (src_params, src_tb) = {
        let st = ictx.stream(stream_index)
            .ok_or_else(|| Error::Ffmpeg(format!("audio stream {stream_index} vanished")))?;
        (st.parameters(), st.time_base())
    };
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(src_params)
        .map_err(|e| Error::Ffmpeg(format!("decoder params: {e}")))?
        .decoder().audio()
        .map_err(|e| Error::Ffmpeg(format!("audio decoder: {e}")))?;
    let rate = decoder.rate();
    let src_layout = decoder.channel_layout();
    let in_ch = src_layout.channels().max(1) as usize;
    if mix.is_some() && in_ch != 4 {
        return Err(Error::Ffmpeg(format!("FOA mix needs 4 input channels, stream has {in_ch}")));
    }
    // Decoded frames → packed f32 at the source rate (format change only).
    let mut to_f32 = ffmpeg::software::resampling::Context::get(
        decoder.format(), src_layout, rate,
        Sample::F32(Type::Packed), src_layout, rate,
    ).map_err(|e| Error::Ffmpeg(format!("resampler: {e}")))?;

    let (codec_id, enc_fmt, enc_layout, out_ch, bit_rate) = match out {
        AudioOut::AacStereo { bit_rate } =>
            (ffmpeg::codec::Id::AAC, Sample::F32(Type::Planar), ChannelLayout::STEREO, 2usize, bit_rate),
        AudioOut::Pcm24 { channels } =>
            (ffmpeg::codec::Id::PCM_S24LE, Sample::I32(Type::Packed), ChannelLayout::default(channels), channels as usize, 0),
    };
    if mix.is_none() && out_ch != in_ch {
        return Err(Error::Ffmpeg(format!("channel count {in_ch} → {out_ch} without a mix")));
    }
    let codec = ffmpeg::encoder::find(codec_id)
        .ok_or_else(|| Error::Ffmpeg(format!("no encoder for {codec_id:?}")))?;
    let mut octx = ffmpeg::format::output(&out_path)
        .map_err(|e| Error::Ffmpeg(format!("open output {out_path:?}: {e}")))?;
    let global_header = octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER);
    let (ost_idx, mut encoder) = {
        let mut ost = octx.add_stream(codec)
            .map_err(|e| Error::Ffmpeg(format!("add stream: {e}")))?;
        let mut enc = ffmpeg::codec::context::Context::from_parameters(ost.parameters())
            .map_err(|e| Error::Ffmpeg(format!("encoder params: {e}")))?
            .encoder().audio()
            .map_err(|e| Error::Ffmpeg(format!("audio encoder: {e}")))?;
        enc.set_rate(rate as i32);
        enc.set_format(enc_fmt);
        enc.set_channel_layout(enc_layout);
        enc.set_time_base((1, rate as i32));
        if bit_rate > 0 { enc.set_bit_rate(bit_rate); }
        if global_header { enc.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER); }
        let encoder = enc.open_as(codec)
            .map_err(|e| Error::Ffmpeg(format!("open encoder {codec_id:?}: {e}")))?;
        ost.set_parameters(&encoder);
        (ost.index(), encoder)
    };
    octx.write_header().map_err(|e| Error::Ffmpeg(format!("write_header: {e}")))?;
    let ost_tb = octx.stream(ost_idx).unwrap().time_base();
    let enc_tb = ffmpeg::Rational(1, rate as i32);
    let frame_size = match encoder.frame_size() { 0 => 4096, n => n as usize };

    // Interleaved f32 output samples waiting to be framed.
    let mut pending: Vec<f32> = Vec::with_capacity(frame_size * out_ch * 4);
    let mut written: u64 = 0;
    let mut packet = ffmpeg::Packet::empty();
    let mut drain = |encoder: &mut ffmpeg::encoder::Audio, octx: &mut ffmpeg::format::context::Output| -> Result<()> {
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(ost_idx);
            packet.rescale_ts(enc_tb, ost_tb);
            packet.write_interleaved(octx)
                .map_err(|e| Error::Ffmpeg(format!("write audio packet: {e}")))?;
        }
        Ok(())
    };
    let mut emit = |pending: &mut Vec<f32>, flush: bool, encoder: &mut ffmpeg::encoder::Audio,
                    octx: &mut ffmpeg::format::context::Output, written: &mut u64| -> Result<()> {
        while pending.len() >= frame_size * out_ch || (flush && !pending.is_empty()) {
            let n = (pending.len() / out_ch).min(frame_size);
            let mut frame = ffmpeg::frame::Audio::new(enc_fmt, frame_size, enc_layout);
            frame.set_rate(rate);
            frame.set_pts(Some(*written as i64));
            // Plane access through the raw pointers: ffmpeg sets only
            // `linesize[0]` for planar audio, so `data_mut(c > 0)` is empty.
            match enc_fmt {
                Sample::F32(Type::Planar) => {
                    for c in 0..out_ch {
                        // SAFETY: `Audio::new` allocated `frame_size` samples per plane.
                        let plane = unsafe {
                            std::slice::from_raw_parts_mut((*frame.as_mut_ptr()).data[c] as *mut f32, frame_size)
                        };
                        for i in 0..frame_size {
                            plane[i] = if i < n { pending[i * out_ch + c] } else { 0.0 };
                        }
                    }
                }
                _ => {
                    // SAFETY: packed — one plane of `frame_size × out_ch` samples.
                    let plane = unsafe {
                        std::slice::from_raw_parts_mut((*frame.as_mut_ptr()).data[0] as *mut i32, frame_size * out_ch)
                    };
                    for i in 0..frame_size * out_ch {
                        plane[i] = if i < n * out_ch {
                            (pending[i].clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32
                        } else { 0 };
                    }
                }
            }
            // PCM takes any frame length; AAC wants full frames — a padded
            // last frame is at most 1023 samples (21 ms) of silence.
            if frame_size != 4096 || n == frame_size { frame.set_samples(frame_size); } else { frame.set_samples(n); }
            encoder.send_frame(&frame).map_err(|e| Error::Ffmpeg(format!("send_frame: {e}")))?;
            drain(encoder, octx)?;
            *written += n as u64;
            pending.drain(..n * out_ch);
        }
        Ok(())
    };

    let tb_s = src_tb.numerator() as f64 / src_tb.denominator().max(1) as f64;
    let win_end = dur_s.map(|d| offset_s + d);
    let mut decoded = ffmpeg::frame::Audio::empty();
    let mut f32_frame = ffmpeg::frame::Audio::empty();
    let mut push = |f: &ffmpeg::frame::Audio, pending: &mut Vec<f32>| {
        let n = f.samples();
        let bytes = &f.data(0)[..n * in_ch * 4];
        let src: &[f32] = bytemuck::cast_slice(bytes);
        match mix {
            Some(m) => for i in 0..n {
                let s = &src[i * 4..i * 4 + 4];
                for row in m.iter() {
                    pending.push(row[0] * s[0] + row[1] * s[1] + row[2] * s[2] + row[3] * s[3]);
                }
            },
            None => pending.extend_from_slice(src),
        }
    };
    for (stream, pkt) in ictx.packets() {
        if stream.index() != stream_index { continue; }
        let t = pkt.pts().or(pkt.dts()).unwrap_or(0) as f64 * tb_s;
        if t < offset_s - 0.05 { continue; }
        if let Some(end) = win_end { if t >= end { break; } }
        if decoder.send_packet(&pkt).is_err() { continue; }
        while decoder.receive_frame(&mut decoded).is_ok() {
            if to_f32.run(&decoded, &mut f32_frame).is_ok() && f32_frame.samples() > 0 {
                push(&f32_frame, &mut pending);
            }
        }
        emit(&mut pending, false, &mut encoder, &mut octx, &mut written)?;
    }
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if to_f32.run(&decoded, &mut f32_frame).is_ok() && f32_frame.samples() > 0 {
            push(&f32_frame, &mut pending);
        }
    }
    if to_f32.flush(&mut f32_frame).is_ok() && f32_frame.samples() > 0 {
        push(&f32_frame, &mut pending);
    }
    emit(&mut pending, true, &mut encoder, &mut octx, &mut written)?;
    encoder.send_eof().map_err(|e| Error::Ffmpeg(format!("send_eof: {e}")))?;
    drain(&mut encoder, &mut octx)?;
    octx.write_trailer().map_err(|e| Error::Ffmpeg(format!("write_trailer: {e}")))?;
    Ok(written)
}

/// Decode the X6's 4-channel 360° Audio (first-order AmbiX) to a
/// front-facing stereo AAC `.m4a` — see [`FOA_STEREO_MATRIX`]. The whole
/// track when `dur_s` is `None`, so the caller can keep muxing with the
/// source's trim offsets. Returns the number of samples written.
pub fn foa_to_stereo_aac(input: &Path, output_m4a: &Path, offset_s: f64, dur_s: Option<f64>, bit_rate: usize) -> Result<u64> {
    let info = probe_foa_aac(input)?
        .ok_or_else(|| Error::Ffmpeg(format!("no 4-channel AAC (360° Audio) track in {input:?}")))?;
    let n = transcode_audio_window(
        input, info.stream_index, output_m4a, offset_s, dur_s,
        Some(&FOA_STEREO_MATRIX), AudioOut::AacStereo { bit_rate },
    )?;
    tracing::info!(m4a = %output_m4a.display(), samples = n, sample_rate = info.sample_rate,
        "X6 360° Audio: decoded first-order AmbiX to a front-facing stereo AAC");
    Ok(n)
}

/// Pick the most useful audio stream in `path` — first non-attached-pic
/// audio track, regardless of channel count or codec. Returns the
/// stream index, channel count, and sample rate. `None` if the file
/// has no audio.
pub fn probe_any_audio(input: &Path) -> Result<Option<(usize, u32, u32)>> {
    crate::decode::init();
    // concat-aware so a multi-segment `.ffconcat` playlist probes the
    // first audio of the chain (preview player passes the playlist).
    let ictx = open_input_concat_aware(input)
        .map_err(|e| Error::Ffmpeg(format!("open {input:?}: {e}")))?;
    for stream in ictx.streams() {
        let params = stream.parameters();
        if params.medium() != ffmpeg::media::Type::Audio { continue; }
        let (channels, sample_rate) = unsafe {
            let raw = params.as_ptr();
            (
                (*raw).ch_layout.nb_channels as u32,
                (*raw).sample_rate as u32,
            )
        };
        return Ok(Some((stream.index(), channels, sample_rate)));
    }
    Ok(None)
}

/// Mux a video-only file with the audio track from a separate source.
/// Both video and audio are copied with no re-encode (stream copy), so
/// the operation is fast and lossless.
///
/// Use this as a post-export step: write the video-only output via
/// `H265Encoder`, then call this to fold the source clip's audio (e.g.
/// the stereo AAC track on DJI OSV) onto the final container.
///
/// Returns the number of audio packets copied. If `audio_src` has no
/// audio track the function returns `Ok(0)` and the output still
/// contains the video — caller can decide whether to keep using it.
/// Open an input, transparently handling an ffconcat playlist (the
/// multi-segment `.360` audio source). The concat demuxer needs
/// `safe=0` to accept the absolute segment paths; a normal file opens
/// plainly. Detected by the `.ffconcat` extension.
pub fn open_input_concat_aware(
    path: &Path,
) -> std::result::Result<ffmpeg::format::context::Input, ffmpeg::Error> {
    if path.extension().and_then(|e| e.to_str()) == Some("ffconcat") {
        let mut d = ffmpeg::Dictionary::new();
        d.set("safe", "0");
        ffmpeg::format::input_with_dictionary(&path, d)
    } else {
        ffmpeg::format::input(&path)
    }
}

/// Write an ffconcat playlist for a multi-segment chain to a temp file
/// so the segments present as ONE continuous audio timeline via the
/// concat demuxer. Returns `None` for a single segment (open it
/// directly). `tag` disambiguates concurrent playlists (export vs
/// preview). Caller deletes the returned temp when done.
pub fn build_segment_playlist(segments: &[std::path::PathBuf], tag: &str) -> Option<std::path::PathBuf> {
    if segments.len() <= 1 { return None; }
    let mut pl = String::from("ffconcat version 1.0\n");
    for s in segments {
        // ffconcat single-quote escaping: ' → '\''
        let esc = s.display().to_string().replace('\'', "'\\''");
        pl.push_str(&format!("file '{esc}'\n"));
    }
    let path = std::env::temp_dir().join(format!("vr180_segs_{tag}.ffconcat"));
    std::fs::write(&path, pl).ok()?;
    Some(path)
}

pub fn mux_video_with_passthrough_audio(
    audio_src: &Path,
    video_src: &Path,
    final_out: &Path,
    audio_offset_s: f64,
    max_duration_s: Option<f64>,
) -> Result<u64> {
    mux_video_with_audio_stream(audio_src, video_src, final_out, audio_offset_s, max_duration_s, None)
}

/// Like [`mux_video_with_passthrough_audio`] but lets the caller pick a
/// SPECIFIC audio stream index (e.g. the 4-channel ambisonic track of a
/// `.360`, which isn't the first audio). `None` = first audio stream.
pub fn mux_video_with_audio_stream(
    audio_src: &Path,
    video_src: &Path,
    final_out: &Path,
    audio_offset_s: f64,
    max_duration_s: Option<f64>,
    audio_stream_idx: Option<usize>,
) -> Result<u64> {
    // `audio_offset_s` / `max_duration_s` implement TRIMMED exports: the
    // video temp starts at source-time `trim_in` but its own timeline at 0,
    // so the source audio must be cut at `audio_offset_s` (= trim_in),
    // rebased to 0, and stopped after the video's duration. Without this a
    // trimmed export carried the FULL source audio at original timestamps —
    // wrong length and out of sync by trim_in seconds.
    crate::decode::init();

    let mut a_in = open_input_concat_aware(audio_src)
        .map_err(|e| Error::Ffmpeg(format!("open audio src {audio_src:?}: {e}")))?;
    let mut v_in = ffmpeg::format::input(&video_src)
        .map_err(|e| Error::Ffmpeg(format!("open video src {video_src:?}: {e}")))?;

    // Locate the audio stream: the caller-specified index (verified to
    // be audio), else the first audio stream.
    let a_idx = match audio_stream_idx {
        Some(i) if a_in.stream(i)
            .map(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
            .unwrap_or(false) => Some(i),
        _ => a_in.streams()
            .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
            .map(|s| s.index())
            .next(),
    };
    let a_idx = match a_idx {
        Some(i) => i,
        None => {
            tracing::info!(
                src = %audio_src.display(),
                "no audio track in source — output will be video-only"
            );
            return Ok(0);
        }
    };

    // Locate the first video stream in the video source.
    let v_idx = v_in.streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
        .map(|s| s.index())
        .next()
        .ok_or_else(|| Error::Ffmpeg(format!("no video stream in {video_src:?}")))?;

    let mut octx = ffmpeg::format::output(&final_out)
        .map_err(|e| Error::Ffmpeg(format!("open output {final_out:?}: {e}")))?;

    // Build output streams (params copy) for both video and audio.
    // We DO NOT clear `codec_tag` for the video stream — the encoder
    // wrote `hvc1` (QuickTime / Vision Pro require it for HEVC) and
    // zeroing it makes the muxer fall back to `hev1`, which those
    // players refuse to decode (the file looks "audio only" in them).
    // For audio we still clear so the muxer picks the right MOV/MP4
    // codec_tag for AAC.
    let v_src_params = v_in.stream(v_idx).unwrap().parameters();
    let v_src_tb = v_in.stream(v_idx).unwrap().time_base();
    let out_v_idx = {
        let mut s = octx.add_stream(ffmpeg::codec::Id::None)
            .map_err(|e| Error::Ffmpeg(format!("add video stream: {e}")))?;
        s.set_parameters(v_src_params);
        s.set_time_base(v_src_tb);
        s.index()
    };

    let a_src_params = a_in.stream(a_idx).unwrap().parameters();
    let a_src_tb = a_in.stream(a_idx).unwrap().time_base();
    let out_a_idx = {
        let mut s = octx.add_stream(ffmpeg::codec::Id::None)
            .map_err(|e| Error::Ffmpeg(format!("add audio stream: {e}")))?;
        s.set_parameters(a_src_params);
        s.set_time_base(a_src_tb);
        unsafe {
            let raw = s.parameters().as_mut_ptr();
            (*raw).codec_tag = 0;
            // GoPro's 4-ch ambisonic PCM carries an "ambisonic 1" channel
            // layout the MP4/MOV muxer rejects ("unsupported channel
            // layout" → write_trailer fails). Replace it with the plain
            // N-channel native layout so the container accepts it; the
            // SA3D atom (injected after) carries the ambisonic semantics.
            let nch = (*raw).ch_layout.nb_channels;
            if nch >= 3 {
                ffmpeg::ffi::av_channel_layout_uninit(&mut (*raw).ch_layout);
                ffmpeg::ffi::av_channel_layout_default(&mut (*raw).ch_layout, nch);
            }
        }
        s.index()
    };

    octx.write_header()
        .map_err(|e| Error::Ffmpeg(format!("write_header: {e}")))?;

    // Interleave video and audio packets by ascending PTS so the MP4
    // muxer doesn't have to buffer the whole stream of one type before
    // it sees the other. We use peekable iterators and pick whichever
    // packet is older next.
    let v_out_tb = octx.stream(out_v_idx).unwrap().time_base();
    let a_out_tb = octx.stream(out_a_idx).unwrap().time_base();
    let mut v_iter = v_in.packets().filter(|(s, _)| s.index() == v_idx).peekable();
    let mut a_iter = a_in.packets().filter(|(s, _)| s.index() == a_idx).peekable();
    let mut v_packets = 0u64;
    let mut a_packets = 0u64;

    // Compute seconds for a packet's dts/pts in its source time base.
    fn pkt_seconds(p: &ffmpeg::Packet, tb: ffmpeg::Rational) -> f64 {
        let t = p.dts().or_else(|| p.pts()).unwrap_or(0);
        t as f64 * tb.numerator() as f64 / tb.denominator() as f64
    }

    // Audio cut window in source-audio time, and the pts rebase (ticks)
    // that moves the first kept packet to ≈0 on the output timeline.
    let a_tb_per_s = a_src_tb.denominator() as f64 / a_src_tb.numerator() as f64;
    let a_off_ticks = (audio_offset_s * a_tb_per_s).round() as i64;
    let mut a_done = false;

    loop {
        // Skip leading audio entirely before trim_in (packet granularity,
        // ~21 ms for AAC — inaudible cut slop).
        while let Some((_, p)) = a_iter.peek() {
            if pkt_seconds(p, a_src_tb) < audio_offset_s - 1e-6 {
                a_iter.next();
            } else {
                break;
            }
        }
        // Stop audio once the video's duration is covered.
        if !a_done {
            if let (Some(max_d), Some((_, p))) = (max_duration_s, a_iter.peek()) {
                if pkt_seconds(p, a_src_tb) - audio_offset_s >= max_d {
                    a_done = true;
                }
            }
        }
        let v_t = v_iter.peek().map(|(_, p)| pkt_seconds(p, v_src_tb));
        let a_t = if a_done { None } else {
            // Compare on the OUTPUT timeline (audio rebased by trim_in).
            a_iter.peek().map(|(_, p)| pkt_seconds(p, a_src_tb) - audio_offset_s)
        };
        let take_video = match (v_t, a_t) {
            (Some(vt), Some(at)) => vt <= at,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_video {
            let (_, mut packet) = v_iter.next().unwrap();
            packet.set_stream(out_v_idx);
            packet.set_position(-1);
            packet.rescale_ts(v_src_tb, v_out_tb);
            packet.write_interleaved(&mut octx)
                .map_err(|e| Error::Ffmpeg(format!("write video packet: {e}")))?;
            v_packets += 1;
        } else {
            let (_, mut packet) = a_iter.next().unwrap();
            // Rebase to the output timeline (clamp the first packet's
            // sub-packet remainder to 0 rather than going negative).
            if let Some(pts) = packet.pts() {
                packet.set_pts(Some((pts - a_off_ticks).max(0)));
            }
            if let Some(dts) = packet.dts() {
                packet.set_dts(Some((dts - a_off_ticks).max(0)));
            }
            packet.set_stream(out_a_idx);
            packet.set_position(-1);
            packet.rescale_ts(a_src_tb, a_out_tb);
            packet.write_interleaved(&mut octx)
                .map_err(|e| Error::Ffmpeg(format!("write audio packet: {e}")))?;
            a_packets += 1;
        }
    }

    octx.write_trailer()
        .map_err(|e| Error::Ffmpeg(format!("write_trailer: {e}")))?;

    tracing::info!(
        final = %final_out.display(),
        video_packets = v_packets,
        audio_packets = a_packets,
        "mux complete — video + audio passthrough"
    );
    Ok(a_packets)
}

#[cfg(test)]
mod foa_tests {
    use super::*;

    #[test]
    fn frontal_source_is_centred_and_left_source_favours_left() {
        // SN3D plane wave from the front: W = 1, X = 1, Y = Z = 0.
        let [l, r] = foa_to_stereo_sample([1.0, 0.0, 0.0, 1.0]);
        assert!((l - r).abs() < 1e-6 && (l - 0.853_553).abs() < 1e-4, "{l} {r}");
        // From the left (+Y): the left cardioid is on axis, the right one at 90°.
        let [l, r] = foa_to_stereo_sample([1.0, 1.0, 0.0, 0.0]);
        assert!(l > r && (l - 0.853_553).abs() < 1e-4 && (r - 0.146_447).abs() < 1e-4, "{l} {r}");
        // From straight above: both cardioids see only W.
        let [l, r] = foa_to_stereo_sample([1.0, 0.0, 1.0, 0.0]);
        assert!((l - 0.5).abs() < 1e-6 && (r - 0.5).abs() < 1e-6);
        // From behind: the pair is 135° off axis — attenuated, still centred.
        let [l, r] = foa_to_stereo_sample([1.0, 0.0, 0.0, -1.0]);
        assert!((l - r).abs() < 1e-6 && l < 0.2, "{l} {r}");
    }
}
