//! 把解密后的 MPEG-TS（H.264 + AAC）封装成普通 MP4。
//!
//! 只处理播放器本来就能播的普通流：H.264 Annex-B 与 ADTS AAC。
//! HEVC、AC-3 等交给调用方改存 `.ts`，不在这里假装能生成通用 MP4。

use std::collections::HashMap;

pub struct Mp4 {
    pub bytes: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

pub fn transmux_ts(ts: &[u8]) -> Result<Mp4, String> {
    let packets = ts_packets(ts)?;
    let units = collect_units(&packets);
    let empty: Vec<Vec<u8>> = Vec::new();
    let pmt_pid = pat_pmt(units.get(&0).unwrap_or(&empty)).ok_or("传输流里没有节目表")?;
    let (video_pid, audio_pid) = pmt_streams(units.get(&pmt_pid).unwrap_or(&empty))
        .ok_or("传输流里没有 H.264 视频")?;

    let video_pes = split_pes(units.get(&video_pid).unwrap_or(&empty));
    let mut video_samples = Vec::new();
    let mut sps = None;
    let mut pps = None;
    for unit in &video_pes {
        for nal in nal_units(&unit.data) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => sps = Some(nal.to_vec()),
                8 => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        let aus = access_units(&unit.data);
        if aus.is_empty() {
            continue;
        }
        for (i, au) in aus.into_iter().enumerate() {
            let key = au.iter().any(|n| !n.is_empty() && n[0] & 0x1f == 5);
            let data = length_prefixed(&au);
            if data.is_empty() {
                continue;
            }
            let (pts, dts) = if i == 0 {
                (unit.pts, unit.dts.or(unit.pts))
            } else {
                (None, None)
            };
            video_samples.push(Sample { data, pts, dts, key });
        }
    }
    if video_samples.is_empty() {
        return Err("没有可封装的视频帧".into());
    }
    let sps = sps.ok_or("缺少 H.264 SPS")?;
    let pps = pps.ok_or("缺少 H.264 PPS")?;
    let (width, height) = sps_dimensions(&sps).unwrap_or((0, 0));
    let (width, height) = if width == 0 || height == 0 { (720, 1280) } else { (width, height) };
    assign_timestamps(&mut video_samples, 3000);

    let audio_pes = audio_pid
        .map(|pid| split_pes(units.get(&pid).unwrap_or(&empty)))
        .unwrap_or_default();
    let mut audio_es = Vec::new();
    for unit in &audio_pes {
        audio_es.extend_from_slice(&unit.data);
    }
    let aac = adts_frames(&audio_es);
    let audio = if let Some(first) = aac.first() {
        let mut samples = Vec::new();
        for frame in &aac {
            samples.push(Sample { data: frame.data.clone(), pts: None, dts: None, key: false });
        }
        // ADTS 帧没有各自的时间戳时，按采样率均匀铺开。
        let step = 1024u64;
        if samples.iter().all(|s| s.pts.is_none()) {
            // 用视频时间轴没有意义，音频用自己的 timescale。
        }
        let base = audio_pes.first().and_then(|u| u.pts).unwrap_or(0);
        for (i, sample) in samples.iter_mut().enumerate() {
            let pts = base + step * i as u64 * 90000 / first.rate as u64;
            sample.pts = Some(pts);
            sample.dts = Some(pts);
        }
        Some(AudioTrack {
            rate: first.rate,
            channels: first.channels,
            profile: first.profile,
            samples,
        })
    } else {
        None
    };

    let bytes = write_mp4(&video_samples, &sps, &pps, width, height, audio.as_ref())?;
    Ok(Mp4 { bytes, width, height })
}

struct Packet {
    pid: u16,
    pusi: bool,
    payload: Vec<u8>,
}

fn ts_packets(ts: &[u8]) -> Result<Vec<Packet>, String> {
    let start = ts.iter().position(|b| *b == 0x47).ok_or("不是 MPEG-TS")?;
    let rest = &ts[start..];
    let stride = if rest.len() > 188 && rest[188] == 0x47 {
        188
    } else if rest.len() > 192 && rest[192] == 0x47 {
        192
    } else if rest.len() >= 188 {
        188
    } else {
        return Err("传输流太短".into());
    };
    let mut out = Vec::new();
    let mut off = 0;
    while off + 188 <= rest.len() {
        let pkt = &rest[off..off + 188];
        if pkt[0] == 0x47 {
            let pid = (((pkt[1] as u16) & 0x1f) << 8) | pkt[2] as u16;
            let pusi = pkt[1] & 0x40 != 0;
            let afc = (pkt[3] >> 4) & 0x03;
            let mut i = 4;
            if afc == 2 || afc == 3 {
                if i >= pkt.len() {
                    off += stride;
                    continue;
                }
                let len = pkt[i] as usize;
                i += 1 + len;
            }
            if (afc == 1 || afc == 3) && i < pkt.len() {
                out.push(Packet { pid, pusi, payload: pkt[i..].to_vec() });
            }
        }
        off += stride;
    }
    if out.is_empty() {
        return Err("没有传输流包".into());
    }
    Ok(out)
}

/// 同一 PID 上，每个 PUSI 开始一个新的载荷（一个 PES 或一节 PSI）。
fn collect_units(packets: &[Packet]) -> HashMap<u16, Vec<Vec<u8>>> {
    let mut out: HashMap<u16, Vec<Vec<u8>>> = HashMap::new();
    for pkt in packets {
        let list = out.entry(pkt.pid).or_default();
        if pkt.pusi || list.is_empty() {
            list.push(Vec::new());
        }
        if let Some(buf) = list.last_mut() {
            buf.extend_from_slice(&pkt.payload);
        }
    }
    out
}

struct Pes {
    pts: Option<u64>,
    dts: Option<u64>,
    data: Vec<u8>,
}

fn split_pes(chunks: &[Vec<u8>]) -> Vec<Pes> {
    chunks.iter().filter_map(|chunk| parse_pes(chunk)).collect()
}

fn parse_pes(payload: &[u8]) -> Option<Pes> {
    if payload.len() >= 6 && payload[0] == 0 && payload[1] == 0 && payload[2] == 1 {
        let stream_id = payload[3];
        // PSI 不是 PES。节目表由单独的函数解析，这里只认音视频流。
        if stream_id < 0xBD {
            return None;
        }
        if payload.len() < 9 {
            return None;
        }
        let flags = payload[7];
        let header_len = payload[8] as usize;
        let data_at = 9 + header_len;
        if data_at > payload.len() {
            return None;
        }
        let pts_dts = (flags >> 6) & 0x03;
        let pts = if pts_dts >= 2 && payload.len() >= 14 {
            Some(read_timestamp(&payload[9..14]))
        } else {
            None
        };
        let dts = if pts_dts == 3 && payload.len() >= 19 {
            Some(read_timestamp(&payload[14..19]))
        } else {
            pts
        };
        return Some(Pes { pts, dts, data: payload[data_at..].to_vec() });
    }
    None
}

fn read_timestamp(d: &[u8]) -> u64 {
    let b0 = d[0] as u64;
    let b1 = d[1] as u64;
    let b2 = d[2] as u64;
    let b3 = d[3] as u64;
    let b4 = d[4] as u64;
    ((b0 & 0x0e) << 29) | (b1 << 22) | ((b2 & 0xfe) << 14) | (b3 << 7) | ((b4 & 0xfe) >> 1)
}

fn psi_sections(chunks: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut sections = Vec::new();
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let pointer = chunk[0] as usize;
        let start = 1 + pointer;
        if start + 3 > chunk.len() {
            continue;
        }
        let sec = &chunk[start..];
        let len = 3 + ((((sec[1] as usize) & 0x0f) << 8) | sec[2] as usize);
        if len <= sec.len() {
            sections.push(sec[..len].to_vec());
        }
    }
    sections
}

fn pat_pmt(chunks: &[Vec<u8>]) -> Option<u16> {
    for sec in psi_sections(chunks) {
        if sec.first().copied() != Some(0) {
            continue;
        }
        let mut i = 8;
        let end = sec.len().saturating_sub(4);
        while i + 4 <= end {
            let program = u16::from_be_bytes([sec[i], sec[i + 1]]);
            let pid = u16::from_be_bytes([sec[i + 2], sec[i + 3]]) & 0x1fff;
            if program != 0 {
                return Some(pid);
            }
            i += 4;
        }
    }
    None
}

fn pmt_streams(chunks: &[Vec<u8>]) -> Option<(u16, Option<u16>)> {
    for sec in psi_sections(chunks) {
        if sec.first().copied() != Some(2) || sec.len() < 12 {
            continue;
        }
        let info_len = (u16::from_be_bytes([sec[10], sec[11]]) & 0x0fff) as usize;
        let mut i = 12 + info_len;
        let end = sec.len().saturating_sub(4);
        let mut video = None;
        let mut audio = None;
        while i + 5 <= end {
            let kind = sec[i];
            let pid = u16::from_be_bytes([sec[i + 1], sec[i + 2]]) & 0x1fff;
            let es_len = (u16::from_be_bytes([sec[i + 3], sec[i + 4]]) & 0x0fff) as usize;
            if kind == 0x1B && video.is_none() {
                video = Some(pid);
            } else if kind == 0x0F && audio.is_none() {
                audio = Some(pid);
            } else if kind == 0x24 {
                return None;
            }
            i += 5 + es_len;
        }
        if let Some(video) = video {
            return Some((video, audio));
        }
    }
    None
}

fn nal_units(es: &[u8]) -> Vec<&[u8]> {
    // (start-code prefix, first byte of the NAL)
    let mut codes = Vec::new();
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            let mut prefix = i;
            while prefix > 0 && es[prefix - 1] == 0 {
                prefix -= 1;
            }
            codes.push((prefix, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, (_, start)) in codes.iter().enumerate() {
        let end = if n + 1 < codes.len() { codes[n + 1].0 } else { es.len() };
        if end > *start {
            out.push(&es[*start..end]);
        }
    }
    out
}

fn access_units<'a>(es: &'a [u8]) -> Vec<Vec<&'a [u8]>> {
    let nals = nal_units(es);
    let mut units = Vec::new();
    let mut cur: Vec<&[u8]> = Vec::new();
    let mut saw_vcl = false;
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let kind = nal[0] & 0x1f;
        let vcl = matches!(kind, 1 | 5);
        if kind == 9 || (vcl && saw_vcl) {
            if !cur.is_empty() {
                units.push(std::mem::take(&mut cur));
            }
            saw_vcl = false;
        }
        if kind != 9 {
            cur.push(nal);
        }
        if vcl {
            saw_vcl = true;
        }
    }
    if !cur.is_empty() {
        units.push(cur);
    }
    units
}

fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        if nal.is_empty() || nal[0] & 0x1f == 9 {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

struct Sample {
    data: Vec<u8>,
    pts: Option<u64>,
    dts: Option<u64>,
    key: bool,
}

fn assign_timestamps(samples: &mut [Sample], fallback: u64) {
    let mut last = 0u64;
    let mut have = false;
    for sample in samples.iter_mut() {
        if let Some(pts) = sample.pts {
            last = pts;
            have = true;
            if sample.dts.is_none() {
                sample.dts = Some(pts);
            }
        } else if have {
            last = last.saturating_add(fallback);
            sample.pts = Some(last);
            sample.dts = Some(last);
        }
    }
    if samples.iter().any(|s| s.dts.is_none()) {
        let mut t = 0;
        for sample in samples.iter_mut() {
            sample.pts.get_or_insert(t);
            sample.dts.get_or_insert(t);
            t += fallback;
        }
    }
}

struct AudioFrame {
    data: Vec<u8>,
    profile: u8,
    rate: u32,
    channels: u8,
}

fn adts_frames(es: &[u8]) -> Vec<AudioFrame> {
    const RATES: [u32; 16] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0, 0];
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 < es.len() {
        if es[i] != 0xFF || es[i + 1] & 0xF0 != 0xF0 {
            i += 1;
            continue;
        }
        let protection_absent = es[i + 1] & 1 == 1;
        let profile = (es[i + 2] >> 6) & 0x03;
        let sf = (es[i + 2] >> 2) & 0x0f;
        let channels = ((es[i + 2] & 1) << 2) | (es[i + 3] >> 6);
        let frame_len = (((es[i + 3] as usize) & 0x03) << 11) | ((es[i + 4] as usize) << 3) | ((es[i + 5] as usize) >> 5);
        let header = if protection_absent { 7 } else { 9 };
        let rate = RATES.get(sf as usize).copied().unwrap_or(0);
        if frame_len < header || rate == 0 || i + frame_len > es.len() {
            break;
        }
        out.push(AudioFrame {
            data: es[i + header..i + frame_len].to_vec(),
            profile,
            rate,
            channels,
        });
        i += frame_len;
    }
    out
}

struct AudioTrack {
    rate: u32,
    channels: u8,
    profile: u8,
    samples: Vec<Sample>,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        Some(((1u32 << zeros) - 1).saturating_add(self.bits(zeros)?))
    }
    fn se(&mut self) -> Option<i32> {
        let u = self.ue()?;
        Some(if u & 1 == 1 { ((u as i32) + 1) / 2 } else { -((u as i32) / 2) })
    }
}

fn rbsp(nal_body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_body.len());
    let mut i = 0;
    while i < nal_body.len() {
        if i + 2 < nal_body.len() && nal_body[i] == 0 && nal_body[i + 1] == 0 && nal_body[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(nal_body[i]);
            i += 1;
        }
    }
    out
}

fn sps_dimensions(nal: &[u8]) -> Option<(u16, u16)> {
    if nal.first().copied().unwrap_or(0) & 0x1f != 7 {
        return None;
    }
    let body = rbsp(&nal[1..]);
    let mut b = Bits { data: &body, pos: 0 };
    let profile = b.bits(8)?;
    let _compat = b.bits(8)?;
    let _level = b.bits(8)?;
    let _id = b.ue()?;
    let high = matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135);
    let mut chroma = 1u32;
    if high {
        chroma = b.ue()?;
        if chroma == 3 {
            let _separate = b.bit()?;
        }
        let _bit_luma = b.ue()?;
        let _bit_chroma = b.ue()?;
        let _qp = b.bit()?;
        if b.bit()? == 1 {
            let lists = if chroma == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_frame = b.ue()?;
    let poc = b.ue()?;
    if poc == 0 {
        let _ = b.ue()?;
    } else if poc == 1 {
        let _ = b.bit()?;
        let _ = b.se()?;
        let _ = b.se()?;
        let n = b.ue()? as usize;
        for _ in 0..n {
            let _ = b.se()?;
        }
    }
    let _refs = b.ue()?;
    let _gaps = b.bit()?;
    let width_mbs = b.ue()? + 1;
    let height_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        let _ = b.bit()?;
    }
    let _direct = b.bit()?;
    let mut width = width_mbs * 16;
    let mut height = height_units * 16 * (2 - frame_mbs_only);
    if b.bit()? == 1 {
        let left = b.ue()?;
        let right = b.ue()?;
        let top = b.ue()?;
        let bottom = b.ue()?;
        let crop_x = if chroma == 0 || chroma == 3 { 1 } else { 2 };
        let crop_y = (if chroma == 1 { 2 } else { 1 }) * (2 - frame_mbs_only);
        width = width.saturating_sub((left + right) * crop_x);
        height = height.saturating_sub((top + bottom) * crop_y);
    }
    if width == 0 || height == 0 || width > 8000 || height > 8000 {
        return None;
    }
    Some((width as u16, height as u16))
}

fn scaling_list(b: &mut Bits, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

fn write_mp4(video: &[Sample], sps: &[u8], pps: &[u8], width: u16, height: u16, audio: Option<&AudioTrack>) -> Result<Vec<u8>, String> {
    let timescale = 90_000u32;
    let video_deltas = deltas(video, 3000);
    let video_duration: u64 = video_deltas.iter().map(|d| *d as u64).sum();
    let (audio_duration, audio_deltas) = if let Some(audio) = audio {
        let step = 1024u32;
        let deltas = vec![step; audio.samples.len()];
        let ticks = step as u64 * audio.samples.len() as u64;
        let mapped = ticks.saturating_mul(timescale as u64) / audio.rate as u64;
        (mapped, deltas)
    } else {
        (0, Vec::new())
    };
    let movie_duration = video_duration.max(audio_duration);
    let ctts = composition_offsets(video, &video_deltas);

    let mut mdat_body = Vec::new();
    let video_offset_in_mdat = 0u32;
    for sample in video {
        mdat_body.extend_from_slice(&sample.data);
    }
    let audio_offset_in_mdat = mdat_body.len() as u32;
    if let Some(audio) = audio {
        for sample in &audio.samples {
            mdat_body.extend_from_slice(&sample.data);
        }
    }

    let ftyp = box4(b"ftyp", &{
        let mut b = Vec::new();
        b.extend_from_slice(b"isom");
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"isomiso2avc1mp41");
        b
    });
    let mut moov_body = Vec::new();
    moov_body.extend(mvhd(timescale, movie_duration, if audio.is_some() { 3 } else { 2 }));
    moov_body.extend(video_trak(video, &video_deltas, &ctts, sps, pps, width, height, timescale, video_duration, 8 + ftyp.len() as u32 + 8 + video_offset_in_mdat));
    if let Some(audio) = audio {
        moov_body.extend(audio_trak(audio, &audio_deltas, 8 + ftyp.len() as u32 + 8 + audio_offset_in_mdat, audio_duration, timescale));
    }
    // mdat 紧跟在 ftyp+moov 后面，所以上面的偏移还没算上 moov 的长度。先占位再回填。
    let moov = box4(b"moov", &moov_body);
    let mdat_header = 8u32;
    let mdat_start = (ftyp.len() + moov.len()) as u32 + mdat_header;
    let video_chunk = mdat_start + video_offset_in_mdat;
    let audio_chunk = mdat_start + audio_offset_in_mdat;
    let mut moov_body = Vec::new();
    moov_body.extend(mvhd(timescale, movie_duration, if audio.is_some() { 3 } else { 2 }));
    moov_body.extend(video_trak(video, &video_deltas, &ctts, sps, pps, width, height, timescale, video_duration, video_chunk));
    if let Some(audio) = audio {
        moov_body.extend(audio_trak(audio, &audio_deltas, audio_chunk, audio_duration, timescale));
    }
    let moov = box4(b"moov", &moov_body);
    let mdat = box4(b"mdat", &mdat_body);
    let mut out = Vec::new();
    out.extend(ftyp);
    out.extend(moov);
    out.extend(mdat);
    Ok(out)
}

fn deltas(samples: &[Sample], fallback: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(samples.len());
    for i in 0..samples.len() {
        let cur = samples[i].dts.unwrap_or(0);
        let next = samples.get(i + 1).and_then(|s| s.dts);
        let d = next.map(|n| n.saturating_sub(cur)).filter(|d| *d > 0 && *d < 90_000).unwrap_or(fallback as u64);
        out.push(d as u32);
    }
    out
}

fn composition_offsets(samples: &[Sample], deltas: &[u32]) -> Vec<u32> {
    let mut dts = 0i64;
    let mut raw = Vec::with_capacity(samples.len());
    for (sample, delta) in samples.iter().zip(deltas) {
        let pts = sample.pts.unwrap_or(dts as u64) as i64;
        raw.push(pts - dts);
        dts += *delta as i64;
    }
    let min = raw.iter().copied().min().unwrap_or(0);
    raw.into_iter().map(|v| (v - min).max(0) as u32).collect()
}

fn video_trak(samples: &[Sample], deltas: &[u32], ctts: &[u32], sps: &[u8], pps: &[u8], width: u16, height: u16, timescale: u32, duration: u64, chunk: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(tkhd(1, duration, width, height, true));
    let mut mdia = Vec::new();
    mdia.extend(mdhd(timescale, duration));
    mdia.extend(hdlr(b"vide", "VideoHandler"));
    let mut minf = Vec::new();
    minf.extend(full_box(b"vmhd", 0, 1, &vec![0u8; 8]));
    minf.extend(dinf());
    minf.extend(stbl_video(samples, deltas, ctts, sps, pps, width, height, chunk));
    mdia.extend(box4(b"minf", &minf));
    body.extend(box4(b"mdia", &mdia));
    box4(b"trak", &body)
}

fn audio_trak(audio: &AudioTrack, deltas: &[u32], chunk: u32, movie_duration: u64, movie_timescale: u32) -> Vec<u8> {
    let duration_samples: u64 = deltas.iter().map(|d| *d as u64).sum();
    let mut body = Vec::new();
    body.extend(tkhd(2, movie_duration, 0, 0, false));
    let mut mdia = Vec::new();
    mdia.extend(mdhd(audio.rate, duration_samples));
    mdia.extend(hdlr(b"soun", "SoundHandler"));
    let mut minf = Vec::new();
    minf.extend(full_box(b"smhd", 0, 0, &[0, 0, 0, 0]));
    minf.extend(dinf());
    minf.extend(stbl_audio(audio, deltas, chunk));
    mdia.extend(box4(b"minf", &minf));
    body.extend(box4(b"mdia", &mdia));
    let _ = movie_timescale;
    box4(b"trak", &body)
}

fn stbl_video(samples: &[Sample], deltas: &[u32], ctts: &[u32], sps: &[u8], pps: &[u8], width: u16, height: u16, chunk: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(stsd_avc(sps, pps, width, height));
    body.extend(stts(deltas));
    if ctts.iter().any(|v| *v != 0) {
        body.extend(ctts_box(ctts));
    }
    body.extend(stsc(samples.len() as u32));
    body.extend(stsz(samples.iter().map(|s| s.data.len() as u32)));
    body.extend(stco(&[chunk]));
    let keys: Vec<u32> = samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
    let keys = if keys.is_empty() { vec![1] } else { keys };
    body.extend(stss(&keys));
    box4(b"stbl", &body)
}

fn stbl_audio(audio: &AudioTrack, deltas: &[u32], chunk: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(stsd_mp4a(audio));
    body.extend(stts(deltas));
    body.extend(stsc(audio.samples.len() as u32));
    body.extend(stsz(audio.samples.iter().map(|s| s.data.len() as u32)));
    body.extend(stco(&[chunk]));
    box4(b"stbl", &body)
}

fn stsd_avc(sps: &[u8], pps: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut avcc = vec![1, sps.get(1).copied().unwrap_or(0x64), sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(0x1f), 0xFF, 0xE1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    let avcc = box4(b"avcC", &avcc);
    // VisualSampleEntry is 78 bytes before child boxes.
    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&[0u8; 12]);
    entry.extend_from_slice(&width.to_be_bytes());
    entry.extend_from_slice(&height.to_be_bytes());
    entry.extend_from_slice(&0x00480000u32.to_be_bytes());
    entry.extend_from_slice(&0x00480000u32.to_be_bytes());
    entry.extend_from_slice(&0u32.to_be_bytes());
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0u8; 32]);
    entry.extend_from_slice(&0x0018u16.to_be_bytes());
    entry.extend_from_slice(&(-1i16).to_be_bytes());
    entry.extend(avcc);
    let entry = box4(b"avc1", &entry);
    let mut stsd = Vec::new();
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend(entry);
    full_box(b"stsd", 0, 0, &stsd)
}

fn stsd_mp4a(audio: &AudioTrack) -> Vec<u8> {
    let object_type = audio.profile + 1;
    let sf_index = match audio.rate {
        96000 => 0, 88200 => 1, 64000 => 2, 48000 => 3, 44100 => 4, 32000 => 5,
        24000 => 6, 22050 => 7, 16000 => 8, 12000 => 9, 11025 => 10, 8000 => 11, 7350 => 12,
        _ => 4,
    };
    let channels = audio.channels.max(1);
    let asc = [(object_type << 3) | (sf_index >> 1), ((sf_index & 1) << 7) | (channels << 3)];
    let dsi = descriptor(0x05, &asc);
    let mut dec = vec![0x40, 0x15, 0, 0, 0];
    dec.extend_from_slice(&128_000u32.to_be_bytes());
    dec.extend_from_slice(&128_000u32.to_be_bytes());
    dec.extend(dsi);
    let dec = descriptor(0x04, &dec);
    let sl = descriptor(0x06, &[0x02]);
    let mut es = Vec::new();
    es.extend_from_slice(&1u16.to_be_bytes());
    es.push(0);
    es.extend(dec);
    es.extend(sl);
    let esds = full_box(b"esds", 0, 0, &descriptor(0x03, &es));
    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0u8; 8]);
    entry.extend_from_slice(&(channels as u16).to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&(audio.rate << 16).to_be_bytes());
    entry.extend(esds);
    let entry = box4(b"mp4a", &entry);
    let mut stsd = Vec::new();
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend(entry);
    full_box(b"stsd", 0, 0, &stsd)
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let mut n = body.len();
    let mut sizes = Vec::new();
    loop {
        sizes.push((n & 0x7f) as u8);
        n >>= 7;
        if n == 0 {
            break;
        }
    }
    for (i, byte) in sizes.iter().rev().enumerate() {
        let last = i + 1 == sizes.len();
        out.push(if last { *byte } else { *byte | 0x80 });
    }
    out.extend_from_slice(body);
    out
}

fn stts(deltas: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for delta in deltas {
        if let Some(last) = runs.last_mut() {
            if last.1 == *delta {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, *delta));
    }
    let mut body = Vec::new();
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&delta.to_be_bytes());
    }
    full_box(b"stts", 0, 0, &body)
}

fn ctts_box(offsets: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for offset in offsets {
        if let Some(last) = runs.last_mut() {
            if last.1 == *offset {
                last.0 += 1;
                continue;
            }
        }
        runs.push((1, *offset));
    }
    let mut body = Vec::new();
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, offset) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&offset.to_be_bytes());
    }
    full_box(b"ctts", 0, 0, &body)
}

fn stsc(sample_count: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&sample_count.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes());
    full_box(b"stsc", 0, 0, &body)
}

fn stsz(sizes: impl Iterator<Item = u32>) -> Vec<u8> {
    let sizes: Vec<u32> = sizes.collect();
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for size in sizes {
        body.extend_from_slice(&size.to_be_bytes());
    }
    full_box(b"stsz", 0, 0, &body)
}

fn stco(chunks: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
    for chunk in chunks {
        body.extend_from_slice(&chunk.to_be_bytes());
    }
    full_box(b"stco", 0, 0, &body)
}

fn stss(keys: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(keys.len() as u32).to_be_bytes());
    for key in keys {
        body.extend_from_slice(&key.to_be_bytes());
    }
    full_box(b"stss", 0, 0, &body)
}

fn dinf() -> Vec<u8> {
    let url = full_box(b"url ", 0, 1, &[]);
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    dref_body.extend(url);
    let dref = full_box(b"dref", 0, 0, &dref_body);
    box4(b"dinf", &dref)
}

fn mvhd(timescale: u32, duration: u64, next: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&(duration as u32).to_be_bytes());
    b.extend_from_slice(&0x00010000u32.to_be_bytes());
    b.extend_from_slice(&0x0100u16.to_be_bytes());
    b.extend_from_slice(&[0u8; 10]);
    b.extend(matrix());
    b.extend_from_slice(&[0u8; 24]);
    b.extend_from_slice(&next.to_be_bytes());
    full_box(b"mvhd", 0, 0, &b)
}

fn tkhd(track_id: u32, duration: u64, width: u16, height: u16, video: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&track_id.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&(duration as u32).to_be_bytes());
    b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(if video { 0u16 } else { 1 }).to_be_bytes());
    b.extend_from_slice(&(if video { 0u16 } else { 0x0100 }).to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend(matrix());
    b.extend_from_slice(&((width as u32) << 16).to_be_bytes());
    b.extend_from_slice(&((height as u32) << 16).to_be_bytes());
    full_box(b"tkhd", 0, 3, &b)
}

fn mdhd(timescale: u32, duration: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&(duration.min(u32::MAX as u64) as u32).to_be_bytes());
    b.extend_from_slice(&0x55C4u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    full_box(b"mdhd", 0, 0, &b)
}

fn hdlr(kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(&[0u8; 12]);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    full_box(b"hdlr", 0, 0, &b)
}

fn matrix() -> Vec<u8> {
    let mut b = Vec::new();
    for v in [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b
}

fn box4(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    out
}

fn full_box(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(4 + body.len());
    inner.push(version);
    inner.extend_from_slice(&flags.to_be_bytes()[1..]);
    inner.extend_from_slice(body);
    box4(typ, &inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_h264_aac_becomes_playable_mp4() {
        let ffmpeg = std::process::Command::new("ffmpeg").arg("-version").output();
        if ffmpeg.is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("hghub-remux-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ts = dir.join("in.ts");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=30",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100",
                "-t", "1", "-shortest",
                "-c:v", "libx264", "-profile:v", "high", "-pix_fmt", "yuv420p", "-g", "30", "-bf", "0",
                "-c:a", "aac", "-b:a", "96k",
                "-f", "mpegts",
            ])
            .arg(&ts)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg fixture");
        let bytes = std::fs::read(&ts).unwrap();
        let mp4 = transmux_ts(&bytes).expect("remux");
        assert!(mp4.width >= 320 && mp4.height >= 240, "{}x{}", mp4.width, mp4.height);
        let out = dir.join("out.mp4");
        std::fs::write(&out, &mp4.bytes).unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args(["-hide_banner", "-show_streams", "-show_format"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout);
        let err = String::from_utf8_lossy(&probe.stderr);
        assert!(probe.status.success(), "{err}");
        assert!(text.contains("codec_name=h264"), "{text}");
        assert!(text.contains("codec_name=aac"), "{text}");
        assert!(text.contains("codec_type=video"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
