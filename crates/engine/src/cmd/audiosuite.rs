//! AudioSuite-style offline processing of selected clips.
use super::{CommandSpec, bad, clip_ids_param, has_selection, range_param, str_param, tracks_param};
use soundcraft_time::Range;

use crate::{Engine, Result, cmd};
use craft_audio::AudioBuffer;
use serde_json::{Value, json};
use soundcraft_dsp::offline::{SpeedRampChange, SpeedRampSelection};
use soundcraft_model::FileFormat;
use soundcraft_model::{Clip, ClipContent, TrackKind};

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "audiosuite.process",
        "AudioSuite Process",
        [],
        None,
        "{process: <built-in plugin id (engine.plugins)> | normalize | reverse | invert | gain | duplicate | time_stretch | pitch_shift | varispeed | speed_ramp | signal_generator, params?: {..}, clips?, tracks?, start?, end?} — signal_generator fills start..end (default: the selection) with a new clip on each audio track in `tracks` (default: the selected tracks); speed_ramp takes change (0 slow down, 1 speed up), selection (0 fit to, 1 extend) and fades (1 on, 0 off)",
        has_selection,
        process
    )]
}

/// The most frames a process can take in: these processes cut their output at MAX_OFFLINE_SAMPLES frames.
fn frame_limit(name: &str, ratio: f64, semitones: f32, sr: f64, to: u32, selection: SpeedRampSelection) -> usize {
    let longest = |grows: f64| (soundcraft_dsp::offline::MAX_OFFLINE_SAMPLES as f64 / grows) as usize;
    match name {
        "time_stretch" => longest(ratio),
        "pitch_shift" => longest(2f64.powf(f64::from(semitones) / 12.0)),
        "varispeed" => longest(f64::from(to) / sr),
        "speed_ramp" => longest(if selection == SpeedRampSelection::Extend { 1.5 } else { 1.0 }),
        _ => usize::MAX,
    }
}

/// Process each target clip's audio and replace the clip with one backed by a new source.
fn process(e: &mut Engine, p: &Value) -> Result<Value> {
    let name = str_param(p, "process").or_else(|| str_param(p, "plugin")).ok_or_else(|| bad("audiosuite.process", "`process` required"))?.to_string();
    let params = params_param(&name, p)?;
    let get = |k: &str, d: f32| params.iter().find(|(n, _)| n == k).map_or(d, |(_, v)| *v);
    let known = soundcraft_dsp::plugin_info(&name).is_some()
        || matches!(
            name.as_str(),
            "normalize"
                | "reverse"
                | "invert"
                | "gain"
                | "duplicate"
                | "time_stretch"
                | "pitch_shift"
                | "varispeed"
                | "speed_ramp"
                | "dc_offset_removal"
        );
    if !known && soundcraft_mix::is_third_party(&name) {
        return Err(bad("audiosuite.process", format!("`{name}` is a hosted plugin; AudioSuite runs built-in plugins only (engine.plugins)")));
    }
    if !known {
        return Err(bad("audiosuite.process", format!("unknown process `{name}`")));
    }
    if soundcraft_dsp::is_generator(&name) && p.get("clips").is_none() && p.get("clip").is_none() {
        let r = range_param(e, "audiosuite.process", p)?;
        if !r.is_empty() {
            return generate(e, p, &name, &params, r);
        }
    }
    let ids = clip_ids_param(e, p);
    if ids.is_empty() {
        return Err(bad("audiosuite.process", "select audio clips first"));
    }
    let sr = e.session().sample_rate;
    let ratio = f64::from(get("ratio", 1.0).clamp(0.25, 4.0));
    let semitones = get("semitones", 0.0).clamp(-24.0, 24.0);
    let to = (sr.as_f64() / f64::from(get("speed", 1.0).clamp(0.25, 4.0))).round().max(1.0) as u32;
    let change = if get("change", 0.0) >= 0.5 { SpeedRampChange::SpeedUp } else { SpeedRampChange::SlowDown };
    let selection = if get("selection", 0.0) >= 0.5 { SpeedRampSelection::Extend } else { SpeedRampSelection::FitTo };
    let fades = get("fades", 1.0) >= 0.5;
    let most = frame_limit(&name, ratio, semitones, sr.as_f64(), to, selection);
    let mut done = 0;
    for id in ids {
        let Some((track, clip)) = e.session().find_clip(id).map(|(t, c)| (t, c.clone())) else { continue };
        if !clip.is_audio() {
            continue;
        }
        let channels = e.session().track(track).map_or(1, |t| t.channels());
        crate::io::refuse_longer(e.session(), "audiosuite.process", &clip.name, "process", clip.length, most.min(crate::io::render_limit(channels)))?;
        let mut audio = soundcraft_mix::render_clips(e.session(), track, clip.range());
        // Render without the clip's own fades: re-render a copy without fades so they stay editable.
        let mut tmp = e.session().clone();
        if let Some(c) = tmp.find_clip_mut(id) {
            c.fade_in = Default::default();
            c.fade_out = Default::default();
            c.gain_db = 0.0;
            c.gain_env.clear();
        }
        if let Some(pl) = tmp.track_mut(track).and_then(|t| t.playlist_mut()) {
            pl.clips.retain(|c| c.id == id);
            audio = soundcraft_mix::render_clips(&tmp, track, clip.range());
        }
        let srf = sr.as_f64() as f32;
        let out: Vec<Vec<f32>> = match name.as_str() {
            "normalize" => {
                soundcraft_dsp::offline::normalize(&mut audio, get("target_db", -0.1), false);
                audio
            }
            "reverse" => {
                soundcraft_dsp::offline::reverse(&mut audio);
                audio
            }
            "invert" => {
                soundcraft_dsp::offline::invert(&mut audio);
                audio
            }
            "gain" if soundcraft_dsp::plugin_info("gain").is_none() || params.iter().any(|(k, _)| k == "db") => {
                soundcraft_dsp::offline::gain(&mut audio, get("db", 0.0));
                audio
            }
            "duplicate" => audio,
            "time_stretch" => soundcraft_dsp::offline::time_stretch(&audio, ratio, srf),
            "pitch_shift" => soundcraft_dsp::offline::pitch_shift(&audio, semitones, srf),
            "varispeed" => soundcraft_dsp::offline::resample(&audio, sr.hz(), to),
            "speed_ramp" => soundcraft_dsp::offline::speed_ramp(&audio, change, selection, fades),
            plugin => {
                apply(plugin, &params, &mut audio, srf)?;
                audio
            }
        };
        let frames = out.first().map_or(0, Vec::len);
        if frames == 0 {
            continue;
        }
        let buf = AudioBuffer { sample_rate: sr.hz(), channels: out };
        let new_name = format!("{}-{}", clip.name, short(&name));
        let s = e.session_mut();
        let src = crate::io::add_source(s, &new_name, buf, None, FileFormat::Wav);
        let mut nc: Clip = clip.clone();
        nc.name = new_name;
        nc.content = ClipContent::Audio { source: src, offset: 0 };
        nc.length = i64::try_from(frames).unwrap_or(nc.length);
        nc.clamp_fades();
        // The render already has the clip's stretch applied; keeping it would stretch the new file again.
        nc.stretch = 1.0;
        // Invert and Duplicate are phase/copy operations. The clip's gain stays a clip gain,
        // the same way its fades stay editable. Other processes bake a new file at unity gain.
        if !matches!(name.as_str(), "invert" | "duplicate") {
            nc.gain_db = 0.0;
            nc.gain_env.clear();
        }
        if let Some(pl) = s.track_mut(track).and_then(|t| t.playlist_mut()) {
            pl.clips.retain(|c| c.id != id);
        }
        crate::edit::place_clip(s, track, nc);
        done += 1;
    }
    Ok(json!({"processed": done, "process": name}))
}

/// Fill `r` on each selected audio track with a new clip from the generator `name`, over
/// whatever was there, like Pro Tools' Signal Generator.
fn generate(e: &mut Engine, p: &Value, name: &str, params: &[(String, f32)], r: Range) -> Result<Value> {
    let sr = e.session().sample_rate;
    let tracks = tracks_param(e, "audiosuite.process", p)?;
    let mut budget = soundcraft_mix::MAX_RENDER_SAMPLES;
    // Refuse before generating anything, rather than cut the clip short.
    for t in tracks.iter().filter_map(|t| e.session().track(*t)).filter(|t| t.kind == TrackKind::Audio) {
        let channels = t.channels().max(1);
        crate::io::refuse_longer(e.session(), "audiosuite.process", &t.name, "generate on", r.len(), crate::io::render_limit(channels))?;
        // Each track keeps its generated buffer for the life of the command (and undo holds it), so
        // cap the total samples generated across tracks, not just each one.
        let frames = usize::try_from(r.len()).unwrap_or(0).min(crate::io::render_limit(channels));
        budget = budget
            .checked_sub(frames.saturating_mul(channels))
            .ok_or_else(|| bad("audiosuite.process", "too much audio to generate in one command"))?;
    }
    let mut clips = Vec::new();
    for track in tracks {
        let Some(t) = e.session().track(track).filter(|t| t.kind == TrackKind::Audio) else { continue };
        let channels = t.channels().max(1);
        let frames = usize::try_from(r.len()).unwrap_or(0).min(crate::io::render_limit(channels));
        let clip_name = format!("{}-{}", t.name, short(name));
        let mut audio = vec![vec![0.0; frames]; channels];
        apply(name, params, &mut audio, sr.as_f64() as f32)?;
        let s = e.session_mut();
        let src = crate::io::add_source(s, &clip_name, AudioBuffer { sample_rate: sr.hz(), channels: audio }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, track, Clip::audio(id, &clip_name, src, 0, r.start, i64::try_from(frames).unwrap_or(0)));
        clips.push(id.0);
    }
    if clips.is_empty() {
        return Err(bad("audiosuite.process", "select an audio track to generate on"));
    }
    Ok(json!({"processed": clips.len(), "process": name, "clips": clips}))
}

/// One param a process takes: id, range and, for a choice, its labels (the value is the index).
struct ParamSpec {
    id: &'static str,
    min: f32,
    max: f32,
    choices: &'static [&'static str],
}

impl ParamSpec {
    const fn any(id: &'static str) -> Self {
        Self { id, min: f32::NEG_INFINITY, max: f32::INFINITY, choices: &[] }
    }
    const fn range(id: &'static str, min: f32, max: f32) -> Self {
        Self { id, min, max, choices: &[] }
    }

    /// How the help and errors name it: `freq (20 to 20000)`, `waveform (Sine | Square | ...)`.
    fn describe(&self) -> String {
        if !self.choices.is_empty() {
            format!("{} ({})", self.id, self.choices.iter().enumerate().map(|(i, c)| format!("{i} {c}")).collect::<Vec<_>>().join(" | "))
        } else if self.min.is_finite() && self.max.is_finite() {
            format!("{} ({} to {})", self.id, self.min, self.max)
        } else {
            self.id.to_string()
        }
    }

    /// A JSON value as this param's number: a number in range, true / false, or a choice's label.
    fn value(&self, process: &str, v: &Value) -> Result<f32> {
        let n = match v {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Number(n) => n.as_f64().filter(|x| x.is_finite()).map(|x| x as f32),
            Value::String(s) => {
                let key = |t: &str| t.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
                self.choices.iter().position(|c| key(c) == key(s)).map(|i| i as f32)
            }
            _ => None,
        };
        let Some(n) = n else {
            let want = if self.choices.is_empty() { "a number".to_string() } else { format!("one of {}", self.choices.join(", ")) };
            return Err(bad("audiosuite.process", format!("`{process}` param `{}` must be {want}, not {v}", self.id)));
        };
        let whole = self.choices.is_empty() || n.fract() == 0.0;
        if n < self.min || n > self.max || !whole {
            return Err(bad("audiosuite.process", format!("`{process}` param {} is out of range: {v}", self.describe())));
        }
        Ok(n)
    }
}

/// The params process `name` takes.
fn param_specs(name: &str) -> Vec<ParamSpec> {
    let mut specs: Vec<ParamSpec> = soundcraft_dsp::plugin_info(name)
        .map(|info| info.params.iter().map(|p| ParamSpec { id: p.id, min: p.min, max: p.max, choices: p.choices }).collect())
        .unwrap_or_default();
    match name {
        "normalize" => specs.push(ParamSpec::any("target_db")),
        "gain" => specs.push(ParamSpec::any("db")),
        "time_stretch" => specs.push(ParamSpec::range("ratio", 0.25, 4.0)),
        "pitch_shift" => specs.push(ParamSpec::range("semitones", -24.0, 24.0)),
        "varispeed" => specs.push(ParamSpec::range("speed", 0.25, 4.0)),
        "speed_ramp" => specs.extend([
            ParamSpec { id: "change", min: 0.0, max: 1.0, choices: &["Slow Down", "Speed Up"] },
            ParamSpec { id: "selection", min: 0.0, max: 1.0, choices: &["Fit To", "Extend"] },
            ParamSpec { id: "fades", min: 0.0, max: 1.0, choices: &["Off", "On"] },
        ]),
        _ => {}
    }
    specs
}

/// `params` checked against what process `name` takes: an unknown name or a bad value is an error that
/// lists the right ones, rather than a setting quietly left at its default.
fn params_param(name: &str, p: &Value) -> Result<Vec<(String, f32)>> {
    let specs = param_specs(name);
    let takes = || {
        if specs.is_empty() {
            format!("`{name}` takes no params")
        } else {
            format!("`{name}` params: {}", specs.iter().map(ParamSpec::describe).collect::<Vec<_>>().join(", "))
        }
    };
    // a param given beside `process` rather than inside `params` would be ignored
    if let Some(s) = specs.iter().find(|s| p.get(s.id).is_some()) {
        return Err(bad("audiosuite.process", format!("put `{}` inside `params`; {}", s.id, takes())));
    }
    let Some(given) = p.get("params").filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
    let Some(given) = given.as_object() else {
        return Err(bad("audiosuite.process", format!("`params` must be an object; {}", takes())));
    };
    let mut out = Vec::new();
    for (k, v) in given {
        let Some(spec) = specs.iter().find(|s| s.id == k) else {
            return Err(bad("audiosuite.process", format!("unknown param `{k}`; {}", takes())));
        };
        out.push((k.clone(), spec.value(name, v)?));
    }
    Ok(out)
}

/// Run plugin `id` with `params` over `audio`, in place.
fn apply(id: &str, params: &[(String, f32)], audio: &mut [Vec<f32>], srf: f32) -> Result<()> {
    let Some(mut pl) = soundcraft_dsp::create(id) else { return Err(bad("audiosuite.process", format!("cannot create `{id}`"))) };
    pl.prepare(srf, 1024, audio.len().max(1));
    for (k, v) in params {
        pl.set_param(k, *v);
    }
    soundcraft_dsp::offline::apply_plugin(audio, pl.as_mut(), srf);
    Ok(())
}

fn short(n: &str) -> String {
    let s: String = n.split('_').map(|w| w.chars().take(4).collect::<String>()).collect::<Vec<_>>().join("");
    s.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_audio::AudioBuffer;
    use soundcraft_model::{ChannelFormat, Session, TrackKind};

    fn gained_clip() -> (crate::Engine, soundcraft_model::ClipId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Tone"));
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 64]] };
        let src = crate::io::add_source(&mut s, "tone", buf, None, FileFormat::Wav);
        let id = s.new_clip_id();
        let mut c = Clip::audio(id, "tone", src, 0, 0, 64);
        c.gain_db = -6.0;
        c.gain_env = vec![(0, 0.0), (32, -12.0)];
        crate::edit::place_clip(&mut s, t, c);
        s.edit.selected_clips = vec![id];
        (crate::Engine::new(s), id)
    }

    #[test]
    fn hosted_plugin_ids_are_refused_with_the_reason() {
        let (mut e, id) = gained_clip();
        for plugin in ["clap:com.example.reverb", "vst3:00112233445566778899AABBCCDDEEFF", "au:aufx:dely:appl"] {
            let err = e.execute("audiosuite.process", &json!({"process": plugin, "clips": [id.0]})).unwrap_err().to_string();
            assert!(err.contains("built-in"), "{plugin}: {err}");
        }
        let spec = specs().into_iter().find(|s| s.id == "audiosuite.process").unwrap();
        assert!(spec.params.contains("<built-in plugin id"), "the help says which ids it takes: {}", spec.params);
    }

    #[test]
    fn invert_and_duplicate_keep_clip_gain() {
        let (mut e, id) = gained_clip();
        e.execute("audiosuite.process", &json!({"process": "invert", "clips": [id.0]})).unwrap();
        let c = e.session().tracks[0].clips()[0].clone();
        assert!((c.gain_db + 6.0).abs() < 1e-6, "{}", c.gain_db);
        assert_eq!(c.gain_env, vec![(0, 0.0), (32, -12.0)]);
        let sample = e.session().pool.get(c.source().unwrap()).unwrap().buffer.channels[0][0];
        assert!((sample + 0.5).abs() < 1e-3, "invert flips the file and leaves the gain on the clip, got {sample}");

        let (mut e, id) = gained_clip();
        e.execute("audiosuite.process", &json!({"process": "duplicate", "clips": [id.0]})).unwrap();
        let c = e.session().tracks[0].clips()[0].clone();
        assert!((c.gain_db + 6.0).abs() < 1e-6, "{}", c.gain_db);
        assert_eq!(c.gain_env, vec![(0, 0.0), (32, -12.0)]);
        let sample = e.session().pool.get(c.source().unwrap()).unwrap().buffer.channels[0][0];
        assert!((sample - 0.5).abs() < 1e-3, "duplicate keeps the file level and the clip gain, got {sample}");
    }

    #[test]
    fn shortening_process_clamps_fades_to_the_new_length() {
        let (mut e, id) = gained_clip();
        {
            let c = e.session_mut().find_clip_mut(id).unwrap();
            c.fade_in.len = 60;
            c.fade_out.len = 40;
        }
        e.execute("audiosuite.process", &json!({"process": "time_stretch", "params": {"ratio": 0.25}, "clips": [id.0]})).unwrap();
        let c = e.session().tracks[0].clips()[0].clone();
        assert!(c.length < 64, "{}", c.length);
        assert!(c.fade_in.len <= c.length, "fade in {} > length {}", c.fade_in.len, c.length);
        assert_eq!(c.fade_out.len, 40.min(c.length));
    }

    #[test]
    fn processed_clip_does_not_keep_the_stretch_it_baked() {
        for stretch in [0.5, 2.0] {
            for process in ["invert", "duplicate"] {
                let (mut e, id) = gained_clip();
                e.session_mut().find_clip_mut(id).unwrap().stretch = stretch;
                e.execute("audiosuite.process", &json!({"process": process, "clips": [id.0]})).unwrap();
                let c = e.session().tracks[0].clips()[0].clone();
                let frames = e.session().pool.get(c.source().unwrap()).unwrap().buffer.channels[0].len();
                assert_eq!(c.stretch, 1.0, "{process} at {stretch}");
                assert_eq!(c.length, i64::try_from(frames).unwrap(), "{process} at {stretch}");
            }
        }
    }

    #[test]
    fn speed_ramp_ramps_the_speed_and_extend_lengthens_the_clip() {
        // A mono clip at 480 whose samples rise from 0 to 1, so each output sample shows where it was read.
        let run = |params: Value| {
            let mut s = Session::default();
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Ramp"));
            let buf = AudioBuffer { sample_rate: 48_000, channels: vec![(0..4800).map(|i| i as f32 / 4800.0).collect()] };
            let src = crate::io::add_source(&mut s, "ramp", buf, None, FileFormat::Wav);
            let id = s.new_clip_id();
            crate::edit::place_clip(&mut s, t, Clip::audio(id, "ramp", src, 0, 480, 4800));
            let mut e = crate::Engine::new(s);
            e.execute("audiosuite.process", &json!({"process": "speed_ramp", "params": params, "clips": [id.0]})).unwrap();
            let c = e.session().tracks[0].clips()[0].clone();
            let audio = e.session().pool.get(c.source().unwrap()).unwrap().buffer.channels[0].clone();
            (c.start, c.length, c.name, audio)
        };
        // The defaults: Slow Down, Fit To, Fades On. Halfway through, the read position is 11/16 of the
        // way through the first two thirds, and the gain is the speed there, 3/4.
        let (start, length, name, audio) = run(json!({}));
        assert_eq!((start, length, name.as_str()), (480, 4800, "ramp-speeramp"));
        let want = (2.0 / 3.0 * 11.0 / 16.0) * 0.75;
        assert!((audio[2400] - want).abs() < 2e-3, "slow down halfway: got {}, want {want}", audio[2400]);
        assert!(audio[4790].abs() < 0.01, "the fade-out ends near silence, got {}", audio[4790]);
        // Speed Up with Extend and no fades: all of the clip is played over 150 % of its length, and the
        // clip still starts at 480.
        let (start, length, _, audio) = run(json!({"change": 1, "selection": 1, "fades": 0}));
        assert_eq!((start, length), (480, 7200));
        assert!((audio[3600] - 5.0 / 16.0).abs() < 2e-3, "speed up halfway: got {}", audio[3600]);
        let r = 1.0 - 7100.0 / 7200.0;
        let want = 1.0 - (3.0 * r - r * r * r) / 2.0;
        assert!((audio[7100] - want).abs() < 2e-3, "speed up near the end: got {}, want {want}", audio[7100]);
    }

    #[test]
    fn signal_generator_fills_the_selection_with_a_clip() {
        let (mut e, _) = gained_clip();
        let tone = e.session().tracks[0].id;
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Empty"));
        e.execute("edit.select", &json!({"tracks": [t.0], "start": 48_000, "end": 96_000})).unwrap();
        assert!(matches!(e.execute("audiosuite.process", &json!({"process": "normalize"})), Err(crate::EngineError::BadParams(..))));

        let generator = json!({"process": "signal_generator", "params": {"freq": 440.0, "level": -6.0}});
        assert_eq!(e.execute("audiosuite.process", &generator).unwrap()["processed"], 1);
        let c = e.session().tracks[1].clips()[0].clone();
        assert_eq!((c.start, c.length), (48_000, 48_000));
        let buf = &e.session().pool.get(c.source().unwrap()).unwrap().buffer;
        assert_eq!(buf.channels.len(), 2);
        let peak = buf.channels[1].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.01, "-6 dB sine peaks near 0.5, got {peak}");

        // Over existing audio it replaces just the selection.
        e.execute("edit.select", &json!({"tracks": [tone.0], "start": 32, "end": 96})).unwrap();
        e.execute("audiosuite.process", &generator).unwrap();
        let ranges: Vec<_> = e.session().tracks[0].clips().iter().map(|c| (c.start, c.length)).collect();
        assert_eq!(ranges, vec![(0, 32), (32, 64)]);
    }

    /// Workbench: `{frequency: 440, amplitude: 0.5, wave: "sine"}` was taken without a word and the
    /// generator ran at its defaults. Wrong names and values are refused, naming the right ones.
    #[test]
    fn wrong_params_are_refused_naming_the_right_ones() {
        let mut e = Engine::new(Session::default());
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Tone"));
        e.execute("edit.select", &json!({"tracks": [t.0], "start": 0, "end": 4800})).unwrap();
        let mut err = |p: Value| e.execute("audiosuite.process", &p).unwrap_err().to_string();
        let msg = err(json!({"process": "signal_generator", "params": {"frequency": 440}}));
        assert!(msg.contains("`frequency`") && msg.contains("waveform") && msg.contains("freq (20 to 20000)") && msg.contains("level"), "{msg}");
        let msg = err(json!({"process": "signal_generator", "params": {"waveform": "kazoo"}}));
        assert!(msg.contains("Sine") && msg.contains("Pink Noise"), "{msg}");
        let msg = err(json!({"process": "signal_generator", "params": {"freq": 50_000}}));
        assert!(msg.contains("out of range"), "{msg}");
        let msg = err(json!({"process": "signal_generator", "params": {"freq": "loud"}}));
        assert!(msg.contains("a number"), "{msg}");
        let msg = err(json!({"process": "signal_generator", "freq": 440}));
        assert!(msg.contains("inside `params`"), "{msg}");
        let msg = err(json!({"process": "signal_generator", "params": [440]}));
        assert!(msg.contains("must be an object"), "{msg}");
        let msg = err(json!({"process": "reverse", "params": {"amount": 1}}));
        assert!(msg.contains("takes no params"), "{msg}");
        let msg = err(json!({"process": "time_stretch", "params": {"ratio": 9}}));
        assert!(msg.contains("ratio (0.25 to 4)"), "{msg}");
        assert!(e.session().tracks[0].clips().is_empty(), "nothing was generated");

        // A choice by its label (any case or spacing), and a toggle as true / false.
        let out = e
            .execute("audiosuite.process", &json!({"process": "signal_generator", "params": {"waveform": "square", "freq": 440, "level": -6}}))
            .unwrap();
        assert_eq!(out["processed"], 1);
        let c = e.session().tracks[0].clips()[0].clone();
        let buf = &e.session().pool.get(c.source().unwrap()).unwrap().buffer;
        let loud = buf.channels[0].iter().filter(|x| x.abs() > 0.45).count();
        assert!(loud > buf.channels[0].len() / 2, "a square wave sits at its peak: {loud} of {}", buf.channels[0].len());
        assert_eq!(params_param("signal_generator", &json!({"params": {"waveform": "white_noise"}})).unwrap(), vec![("waveform".to_string(), 4.0)]);
        assert_eq!(params_param("speed_ramp", &json!({"params": {"fades": false, "change": "Speed Up"}})).unwrap().len(), 2);
    }

    #[test]
    fn duplicate_track_ids_generate_once_not_per_listing() {
        let mut e = Engine::new(Session::default());
        let t = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Tone"));
        e.execute("edit.select", &json!({"tracks": [t.0], "start": 0, "end": 4800})).unwrap();
        // The same track eight times over: it must generate once, not eight times (#896).
        let out = e
            .execute(
                "audiosuite.process",
                &json!({"process": "signal_generator", "tracks": [t.0, t.0, t.0, t.0, t.0, t.0, t.0, t.0], "params": {"freq": 440.0}}),
            )
            .unwrap();
        assert_eq!(out["processed"], 1);
        assert_eq!(e.session().tracks[0].clips().len(), 1, "one clip, not one per duplicate listing");
    }

    #[test]
    fn generate_refuses_a_total_budget_across_tracks() {
        let mut e = Engine::new(Session::default());
        // Two tracks each near the per-track limit: the second would push the total past the budget.
        for n in 0..2 {
            e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some(&format!("T{n}")));
        }
        let t: Vec<_> = e.session().tracks.iter().map(|t| t.id.0).collect();
        // Select both tracks so signal_generator targets them.
        e.execute("edit.select", &json!({"tracks": [t[0], t[1]], "start": 0, "end": 4800})).unwrap();
        e.execute("audiosuite.process", &serde_json::json!({"process": "signal_generator", "track": t[0], "params": {"freq": 440.0}})).unwrap();
        assert_eq!(e.session().tracks[0].clips().len(), 1, "one track generates one clip");
        // Two distinct tracks over the total budget are refused (MAX_RENDER_SAMPLES is f32 samples
        // across all channels; a single track over the limit is already refused per-track).
        let len = (soundcraft_mix::MAX_RENDER_SAMPLES as i64) / 3 * 2;
        let out = e.execute(
            "audiosuite.process",
            &serde_json::json!({"process": "signal_generator", "tracks": [t[0], t[1]], "params": {"freq": 440.0}, "start": 0, "end": len}),
        );
        let ok = matches!(out, Err(crate::EngineError::BadParams(_, ref m)) if m.contains("too much audio"));
        assert!(ok, "two tracks over budget, but got: {out:?}");
    }
}
